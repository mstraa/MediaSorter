use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};

/// The built React frontend (Vite `dist/`). At dev time this folder only
/// contains `.gitkeep`; the production build populates it before `cargo build`.
#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct FrontendAssets;

/// Vite emits content-hashed filenames under `assets/`, so those bytes can
/// never change behind a given URL — cache them for a year. Everything else
/// (`index.html`, `favicon.svg`) keeps a stable URL across builds and must be
/// revalidated, which the ETag makes cheap: a 304 instead of the whole bundle.
const IMMUTABLE_PREFIX: &str = "assets/";
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
const REVALIDATE_CACHE: &str = "no-cache";

pub async fn static_handler(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_index(&headers);
    }
    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cache = if path.starts_with(IMMUTABLE_PREFIX) {
                IMMUTABLE_CACHE
            } else {
                REVALIDATE_CACHE
            };
            serve_embedded(content, mime.as_ref(), cache, &headers)
        }
        // SPA fallback: unknown non-asset routes return index.html so the
        // client-side router can handle them.
        None => serve_index(&headers),
    }
}

fn serve_index(headers: &HeaderMap) -> Response {
    match FrontendAssets::get("index.html") {
        Some(content) => serve_embedded(
            content,
            "text/html; charset=utf-8",
            REVALIDATE_CACHE,
            headers,
        ),
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from(DEV_PLACEHOLDER),
        )
            .into_response(),
    }
}

/// Serve an embedded file with an ETag, answering `If-None-Match` with 304.
///
/// Without this every page load re-sent the whole bundle (a couple of hundred
/// KB) over the LAN, and the body was copied out of the binary each time.
fn serve_embedded(
    content: EmbeddedFile,
    mime: &str,
    cache_control: &str,
    headers: &HeaderMap,
) -> Response {
    let etag = format_etag(&content.metadata.sha256_hash());

    if let Some(requested) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        // `If-None-Match` may carry a list, and a proxy can weaken the tag.
        if requested.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || candidate.trim_start_matches("W/") == etag
        }) {
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag.as_str()),
                    (header::CACHE_CONTROL, cache_control),
                ],
            )
                .into_response();
        }
    }

    let mut response = (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, cache_control),
        ],
        content.data.into_owned(),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn format_etag(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(2 + 32 + 1);
    out.push('"');
    // Half the digest is plenty to distinguish builds and keeps the header small.
    for byte in &hash[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push('"');
    out
}

const DEV_PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>MediaSorter</title></head>
<body style="font-family:sans-serif;padding:2rem">
<h1>MediaSorter API is running</h1>
<p>The frontend bundle is not embedded. In development, run the Vite dev server
(<code>cd frontend &amp;&amp; npm run dev</code>) which proxies the API. In production,
build the frontend (<code>npm run build</code>) before <code>cargo build --release</code>.</p>
</body></html>"#;
