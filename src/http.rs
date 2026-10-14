use crate::pipeline::Supervisor;
use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("../static/index.html");

async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "?".to_string());
    let response = next.run(req).await;
    tracing::info!(%peer, %method, %uri, status = %response.status(), "request");
    response
}

pub fn router(supervisor: Arc<Supervisor>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/playlist.m3u", get(playlist))
        .route("/ch/:id/*file", get(serve_channel_file))
        .with_state(supervisor)
        .layer(middleware::from_fn(log_requests))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn playlist(State(sup): State<Arc<Supervisor>>, headers: HeaderMap) -> impl IntoResponse {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost:8080");

    let mut body = String::from("#EXTM3U\n");
    for ch in sup.channels() {
        body.push_str(&format!(
            "#EXTINF:-1 tvg-id=\"{id}\" tvg-name=\"{name}\" group-title=\"Home IPTV\",{name}\n",
            id = ch.id,
            name = ch.name,
        ));
        body.push_str(&format!("http://{host}/ch/{}/stream.m3u8\n", ch.id));
    }

    // Not "audio/x-mpegurl": some browsers/apps treat that as a direct HLS
    // manifest and try to hand it to a native player instead of fetching
    // it as plain data, which fails since this is a channel list, not a
    // playable stream on its own.
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
}

async fn serve_channel_file(
    State(sup): State<Arc<Supervisor>>,
    Path((id, file)): Path<(String, String)>,
) -> Response {
    if sup.channel_config(&id).is_none() {
        return (StatusCode::NOT_FOUND, "unknown channel").into_response();
    }

    if let Err(err) = sup.ensure_running(&id).await {
        tracing::error!(channel = %id, error = ?err, "failed to start channel pipeline");
        return (StatusCode::INTERNAL_SERVER_ERROR, "pipeline error").into_response();
    }

    // Prevent path traversal outside the channel's own output directory.
    if file.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }

    let path = sup.output_dir(&id).join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let content_type = if file.ends_with(".m3u8") {
                "application/vnd.apple.mpegurl"
            } else if file.ends_with(".ts") {
                "video/mp2t"
            } else {
                "application/octet-stream"
            };
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "segment not ready yet").into_response(),
    }
}
