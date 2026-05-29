use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

// Optional SOCKS proxy (e.g. socks5h://100.64.0.3:1080) prepended to every
// yt-dlp invocation. socks5h keeps DNS on the proxy side too. Set via the
// YTDLP_PROXY env var; absent => yt-dlp egresses directly.
fn proxy_args() -> &'static [String] {
    static ARGS: OnceLock<Vec<String>> = OnceLock::new();
    ARGS.get_or_init(|| match std::env::var("YTDLP_PROXY") {
        Ok(p) if !p.is_empty() => vec!["--proxy".to_string(), p],
        _ => Vec::new(),
    })
}

// Build the full yt-dlp argv: proxy flag (if any) followed by the call-specific
// args. Returned as owned Strings so callers can pass borrowed &str slices.
fn ytdlp_argv(args: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = proxy_args().to_vec();
    argv.extend(args.iter().map(|s| s.to_string()));
    argv
}

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
];

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}

#[derive(Deserialize)]
struct ChannelQuery {
    url: String,
    limit: Option<u32>,
}

// Run yt-dlp, retrying with backoff on failure. A failed invocation can mean a
// genuine bad URL or a transient hiccup reaching the egress proxy (watts may be
// briefly down); rather than hard-failing on the first error we retry a few
// times so a momentarily-unreachable proxy degrades to a slow request, not a
// 502. The last stderr is surfaced if every attempt fails.
async fn run_ytdlp(args: &[&str]) -> Result<serde_json::Value, Response> {
    let argv = ytdlp_argv(args);
    let mut last_err: Option<String> = None;

    for attempt in 0..=RETRY_DELAYS.len() {
        let output = Command::new("yt-dlp")
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                error!("failed to spawn yt-dlp: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn error: {e}")).into_response()
            })?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return serde_json::from_str(&stdout).map_err(|e| {
                error!("json parse error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("json parse error: {e}"),
                )
                    .into_response()
            });
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        last_err = Some(stderr.clone());
        if let Some(delay) = RETRY_DELAYS.get(attempt) {
            warn!(attempt, "yt-dlp failed, retrying: {stderr}");
            tokio::time::sleep(*delay).await;
        }
    }

    let stderr = last_err.unwrap_or_default();
    error!("yt-dlp failed after retries: {stderr}");
    Err((StatusCode::BAD_GATEWAY, format!("yt-dlp error: {stderr}")).into_response())
}

async fn metadata(Query(q): Query<UrlQuery>) -> Result<impl IntoResponse, Response> {
    info!(url = %q.url, "metadata request");

    let json = run_ytdlp(&[
        "--dump-single-json",
        "--no-download",
        "--no-playlist",
        &q.url,
    ])
    .await?;

    Ok(Json(json))
}

async fn audio(Query(q): Query<UrlQuery>) -> Result<impl IntoResponse, Response> {
    info!(url = %q.url, "audio request");

    let id = uuid::Uuid::new_v4().to_string();
    let dir = std::env::temp_dir().join("yt-dlp-api");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir error: {e}")).into_response()
    })?;

    let template = dir.join(format!("{id}.%(ext)s"));
    let template_str = template.to_string_lossy();

    let argv = ytdlp_argv(&[
        "--extract-audio",
        "--audio-format", "opus",
        "--audio-quality", "0",
        "--no-playlist",
        "-o", &template_str,
        &q.url,
    ]);

    let mut last_err: Option<String> = None;
    let mut ok = false;
    for attempt in 0..=RETRY_DELAYS.len() {
        let output = Command::new("yt-dlp")
            .args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn error: {e}")).into_response()
            })?;

        if output.status.success() {
            ok = true;
            break;
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        last_err = Some(stderr.clone());
        if let Some(delay) = RETRY_DELAYS.get(attempt) {
            warn!(attempt, "yt-dlp audio failed, retrying: {stderr}");
            tokio::time::sleep(*delay).await;
        }
    }

    if !ok {
        let stderr = last_err.unwrap_or_default();
        error!("yt-dlp audio failed after retries: {stderr}");
        return Err((StatusCode::BAD_GATEWAY, format!("yt-dlp error: {stderr}")).into_response());
    }

    // Find the output file (extension determined by yt-dlp)
    let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("readdir error: {e}")).into_response()
    })?;

    let mut found = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(&id) {
            found = Some(entry.path());
            break;
        }
    }

    let path = found.ok_or_else(|| {
        (StatusCode::INTERNAL_SERVER_ERROR, "output file not found").into_response()
    })?;

    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response()
    })?;

    // Clean up
    let _ = tokio::fs::remove_file(&path).await;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("opus");

    let content_type = match ext {
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "webm" => "audio/webm",
        _ => "application/octet-stream",
    };

    let disposition = format!("attachment; filename=\"audio.{ext}\"");

    Ok((
        [
            ("content-type".to_string(), content_type.to_string()),
            ("content-disposition".to_string(), disposition),
        ],
        bytes,
    ))
}

async fn playlist(Query(q): Query<UrlQuery>) -> Result<impl IntoResponse, Response> {
    info!(url = %q.url, "playlist request");

    let json = run_ytdlp(&[
        "--dump-single-json",
        "--flat-playlist",
        "--no-download",
        &q.url,
    ])
    .await?;

    Ok(Json(json))
}

async fn channel(Query(q): Query<ChannelQuery>) -> Result<impl IntoResponse, Response> {
    let limit = q.limit.unwrap_or(50);
    info!(url = %q.url, limit, "channel request");

    let limit_str = limit.to_string();
    let json = run_ytdlp(&[
        "--dump-single-json",
        "--flat-playlist",
        "--no-download",
        "--playlist-end", &limit_str,
        "--extractor-args", "youtubetab:skip=authcheck",
        &q.url,
    ])
    .await?;

    Ok(Json(json))
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let app = Router::new()
        .route("/metadata", get(metadata))
        .route("/audio", get(audio))
        .route("/playlist", get(playlist))
        .route("/channel", get(channel))
        .route("/health", get(health))
        .layer(TraceLayer::new_for_http());

    let addr = "0.0.0.0:3000";
    info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
