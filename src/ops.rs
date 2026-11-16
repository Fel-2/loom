//! Operational endpoints and a tiny Prometheus surface. No dependencies:
//! the numbers come from atomics and /proc.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::{header, HeaderValue};
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
