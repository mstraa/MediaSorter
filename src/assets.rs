use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The built React frontend (Vite `dist/`). At dev time this folder only
/// contains `.gitkeep`; the production build populates it before `cargo build`.
#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct FrontendAssets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_index();
    }
    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                content.data.into_owned(),
            )
                .into_response()
        }
        // SPA fallback: unknown non-asset routes return index.html so the
        // client-side router can handle them.
        None => serve_index(),
    }
}

fn serve_index() -> Response {
    match FrontendAssets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from(DEV_PLACEHOLDER),
        )
            .into_response(),
    }
}

const DEV_PLACEHOLDER: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>MediaSorter</title></head>
<body style="font-family:sans-serif;padding:2rem">
<h1>MediaSorter API is running</h1>
<p>The frontend bundle is not embedded. In development, run the Vite dev server
(<code>cd frontend &amp;&amp; npm run dev</code>) which proxies the API. In production,
build the frontend (<code>npm run build</code>) before <code>cargo build --release</code>.</p>
</body></html>"#;
