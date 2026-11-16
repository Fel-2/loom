//! Operational endpoints and a tiny Prometheus surface. No dependencies:
//! the numbers come from atomics and /proc.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::extract::{Request, State};
use http_body::Body as _;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use axum::Json;
use serde_json::json;

use crate::app::App;

async fn healthz(State(app): State<Arc<App>>) -> Response {
    let db_ok = app.db.schema_version() > 0;
    let writable = std::fs::metadata(app.data.join("repos")).map(|m| !m.permissions().readonly()).unwrap_or(false);
    let (status, code) = if db_ok && writable { ("pass", axum::http::StatusCode::OK) } else { ("fail", axum::http::StatusCode::SERVICE_UNAVAILABLE) };
    (code, Json(json!({ "status": status, "checks": { "database": db_ok, "storage": writable } }))).into_response()
}

fn proc_number(name: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

async fn metrics(State(app): State<Arc<App>>) -> Response {
    let s = &app.stats;
    let uptime = crate::db::now() - app.started;
    let repos = app.db.repo_count_total().unwrap_or(0);
    let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0);
    let body = format!(
        "# HELP loom_up 1 when the server is up.\n# TYPE loom_up gauge\nloom_up 1\n\
         # TYPE loom_uptime_seconds gauge\nloom_uptime_seconds {uptime}\n\
         # TYPE loom_requests_total counter\nloom_requests_total {}\n\
         # TYPE loom_requests_4xx_total counter\nloom_requests_4xx_total {}\n\
         # TYPE loom_requests_5xx_total counter\nloom_requests_5xx_total {}\n\
         # TYPE loom_in_flight_requests gauge\nloom_in_flight_requests {}\n\
         # TYPE loom_git_slots_available gauge\nloom_git_slots_available {}\n\
         # TYPE loom_repos gauge\nloom_repos {}\n\
         # TYPE loom_process_resident_memory_bytes gauge\nloom_process_resident_memory_bytes {}\n\
         # TYPE loom_process_threads gauge\nloom_process_threads {threads}\n\
         # TYPE loom_build_info gauge\nloom_build_info{{version=\"{}\"}} 1\n",
        s.requests.load(Ordering::Relaxed),
        s.errors_4xx.load(Ordering::Relaxed),
        s.errors_5xx.load(Ordering::Relaxed),
        s.in_flight.load(Ordering::Relaxed),
        app.git_slots.available_permits(),
        repos,
        proc_number("VmRSS:") * 1024,
        env!("CARGO_PKG_VERSION"),
    );
    let mut res = body.into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"));
    res
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/healthz", get(healthz))
        .route("/metrics", get(metrics))
}

/// Count every request; cheap enough to run on all paths.
pub async fn count(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    app.stats.requests.fetch_add(1, Ordering::Relaxed);
    app.stats.in_flight.fetch_add(1, Ordering::Relaxed);
    let res = next.run(req).await;
    app.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    match res.status().as_u16() {
        400..=499 => app.stats.errors_4xx.fetch_add(1, Ordering::Relaxed),
        500..=599 => app.stats.errors_5xx.fetch_add(1, Ordering::Relaxed),
        _ => 0,
    };
    res
}

/// Token bucket per (ip, class). Burst = 2 seconds' worth.
fn take(app: &App, ip: &str, write: bool) -> bool {
    let rate = if write { app.rate_write } else { app.rate_read };
    if rate <= 0.0 {
        return true;
    }
    let burst = (rate * 2.0).max(1.0);
    let key = format!("{}{ip}", if write { "w:" } else { "r:" });
    let now = std::time::Instant::now();
    let mut b = app.buckets.lock().unwrap_or_else(|e| e.into_inner());
    if b.len() > 50_000 {
        b.retain(|_, v| now.duration_since(v.last).as_secs() < 60);
    }
    let e = b.entry(key).or_insert(crate::app::Bucket { tokens: burst, last: now });
    let elapsed = now.duration_since(e.last).as_secs_f64();
    e.tokens = (e.tokens + elapsed * rate).min(burst);
    e.last = now;
    if e.tokens >= 1.0 {
        e.tokens -= 1.0;
        true
    } else {
        false
    }
}

pub async fn rate_limit(State(app): State<Arc<App>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path();
    // Monitoring and immutable assets are never throttled.
    if path == "/healthz" || path == "/api/healthz" || path == "/metrics" || path.starts_with("/static/") || path.starts_with("/avatar/") {
        return next.run(req).await;
    }
    let write = !matches!(*req.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS);
    let ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into());
    if !take(&app, &ip, write) {
        let mut res = (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "Too many requests; slow down.\n",
        )
            .into_response();
        res.headers_mut().insert(axum::http::header::RETRY_AFTER, axum::http::HeaderValue::from_static("1"));
        return res;
    }
    next.run(req).await
}

const GZIP_MIN: usize = 512;
const GZIP_MAX: usize = 256 * 1024;

fn gzip_wanted(req: &Request) -> bool {
    if req.method() == Method::HEAD {
        return false;
    }
    req.headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|p| p.split(';').next().is_some_and(|s| s.trim().eq_ignore_ascii_case("gzip"))))
}

fn compressible(ct: &str) -> bool {
    let ct = ct.to_ascii_lowercase();
    ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") || ct.contains("javascript") || ct.contains("svg")
}

/// Gzip small text responses. Only bodies sent with a known small
/// `Content-Length` are touched, so streaming git responses pass through
/// untouched.
pub async fn compress(req: Request, next: Next) -> Response {
    let wanted = gzip_wanted(&req);
    let res = next.run(req).await;
    if !wanted || res.status() != StatusCode::OK {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    if parts.headers.contains_key(header::CONTENT_ENCODING) {
        return Response::from_parts(parts, body);
    }
    let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    if !compressible(&ct) {
        return Response::from_parts(parts, body);
    }
    let len = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| body.size_hint().exact().map(|n| n as usize));
    let Some(len) = len else { return Response::from_parts(parts, body) };
    if len < GZIP_MIN || len > GZIP_MAX {
        return Response::from_parts(parts, body);
    }
    let Ok(bytes) = axum::body::to_bytes(body, GZIP_MAX).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(bytes.len() / 2), flate2::Compression::new(6));
    if std::io::Write::write_all(&mut enc, &bytes).is_err() {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let Ok(packed) = enc.finish() else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if packed.len() >= bytes.len() {
        return Response::from_parts(parts, Body::from(bytes));
    }
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    parts.headers.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&packed.len().to_string()).unwrap_or(HeaderValue::from_static("0")));
    parts.headers.append(header::VARY, HeaderValue::from_static("accept-encoding"));
    Response::from_parts(parts, Body::from(packed))
}
