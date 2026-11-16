//! Git LFS batch API (v1): upload/download of large objects, stored under
//! `data/lfs/<repo id>/<aa>/<bb>/<oid>`. The git protocol itself is untouched:
//! LFS pointers travel in the repo, objects come here.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::app::App;
use crate::db::{Access, Repo, User};

const JSON: &str = "application/vnd.git-lfs+json";

fn max_object() -> u64 {
    std::env::var("LOOM_LFS_MAX_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2048) * 1024 * 1024
}

fn lfs_path(app: &App, repo: &Repo, oid: &str) -> PathBuf {
    app.data.join("lfs").join(repo.id.to_string()).join(&oid[..2]).join(&oid[2..4]).join(oid)
}

/// 64 lowercase hex characters.
fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn lfs_json(status: StatusCode, v: Value) -> Response {
    (status, [(header::CONTENT_TYPE, JSON)], v.to_string()).into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    lfs_json(status, json!({ "message": msg }))
}

/// Authorization for LFS: same credentials as git (password or token).
async fn authorize(app: &Arc<App>, headers: &HeaderMap, owner: &str, name: &str, need: Access) -> Result<Repo, Response> {
    let user: Option<User> = match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(v) if v.starts_with("Bearer ") => app
            .verify_lfs_token(v.trim_start_matches("Bearer ").trim())
            .and_then(|id| app.db.user_by_id(id).ok().flatten()),
        _ => crate::githttp::basic_user(app, headers).await,
    };
    if headers.contains_key(header::AUTHORIZATION) && user.is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"loom\", charset=\"UTF-8\"")],
        )
            .into_response());
    }
    app.authorize(user.as_ref(), owner, name, need).map(|(r, _)| r).ok_or_else(|| {
        if user.is_none() {
            err(StatusCode::UNAUTHORIZED, "Authentication required")
        } else {
            err(StatusCode::NOT_FOUND, "Repository not found")
        }
    })
}

/// Absolute URL for an object action; the client reuses the credentials it
/// already sent for this host.
fn href(app: &App, repo: &Repo, rest: &str) -> String {
    let base = std::env::var("LOOM_URL").unwrap_or_else(|_| app.url.clone());
    format!("{}/{}/{}.git/info/lfs/objects/{rest}", base.trim_end_matches('/'), repo.owner, repo.name)
}

#[derive(Deserialize)]
struct BatchObject {
    oid: String,
    #[serde(default)]
    size: i64,
}

#[derive(Deserialize)]
struct BatchRequest {
    operation: String,
    objects: Vec<BatchObject>,
}

const MAX_BATCH: usize = 1000;

async fn batch(State(app): State<Arc<App>>, AxPath((owner, name)): AxPath<(String, String)>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if body.len() > 1024 * 1024 {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "batch request too large");
    }
    let Ok(req) = serde_json::from_slice::<BatchRequest>(&body) else {
        return err(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let upload = req.operation == "upload";
    if !upload && req.operation != "download" {
        return err(StatusCode::BAD_REQUEST, "operation must be upload or download");
    }
    let need = if upload { Access::Write } else { Access::Read };
    let repo = match authorize(&app, &headers, &owner, &name, need).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req.objects.len() > MAX_BATCH {
        return err(StatusCode::BAD_REQUEST, "too many objects in one batch");
    }
    // Echo the caller's credentials on each action: git-lfs does not reuse
    // them for the object URL unless the batch response says to.
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let with_auth = |mut action: Value| -> Value {
        if let Some(a) = &auth {
            action["header"] = json!({ "Authorization": a });
        }
        action
    };
    let mut out = Vec::with_capacity(req.objects.len());
    for o in &req.objects {
        if !valid_oid(&o.oid) || o.size < 0 {
            out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 422, "message": "invalid oid or size" } }));
            continue;
        }
        let path = lfs_path(&app, &repo, &o.oid);
        if upload {
            if o.size as u64 > max_object() {
                out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 413, "message": "object too large" } }));
                continue;
            }
            out.push(json!({
                "oid": o.oid, "size": o.size, "authenticated": true,
                "actions": { "upload": with_auth(json!({ "href": href(&app, &repo, &o.oid), "expires_in": 3600 })) }
            }));
        } else if path.exists() {
            out.push(json!({
                "oid": o.oid, "size": o.size, "authenticated": true,
                "actions": { "download": with_auth(json!({ "href": href(&app, &repo, &o.oid), "expires_in": 3600 })) }
            }));
        } else {
            out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 404, "message": "object not found" } }));
        }
    }
    lfs_json(StatusCode::OK, json!({ "transfer": "basic", "objects": out }))
}

async fn upload(State(app): State<Arc<App>>, AxPath((owner, name, oid)): AxPath<(String, String, String)>, headers: HeaderMap, body: Body) -> Response {
    if !valid_oid(&oid) {
        return err(StatusCode::BAD_REQUEST, "invalid oid");
    }
    let repo = match authorize(&app, &headers, &owner, &name, Access::Write).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let path = lfs_path(&app, &repo, &oid);
    if path.exists() {
        return StatusCode::OK.into_response();
    }
    let Some(parent) = path.parent() else { return err(StatusCode::INTERNAL_SERVER_ERROR, "bad path") };
    let tmp = path.with_extension(format!("tmp-{}", crate::db::now()));
    if let Err(e) = tokio::fs::create_dir_all(parent).await {
        eprintln!("lfs mkdir: {e}");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "storage error");
    }
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("lfs create: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "storage error");
        }
    };
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let limit = max_object();
    let mut stream = body.into_data_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::BAD_REQUEST, "upload interrupted");
        };
        size += chunk.len() as u64;
        if size > limit {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::PAYLOAD_TOO_LARGE, "object too large");
        }
        hasher.update(&chunk);
        if file.write_all(&chunk).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, "write failed");
        }
    }
    if file.flush().await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, "write failed");
    }
    drop(file);
    let got = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != oid {
        let _ = tokio::fs::remove_file(&tmp).await;
        return err(StatusCode::UNPROCESSABLE_ENTITY, "content does not match oid");
    }
    match tokio::fs::rename(&tmp, &path).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            eprintln!("lfs rename: {e}");
            let _ = tokio::fs::remove_file(&tmp).await;
            err(StatusCode::INTERNAL_SERVER_ERROR, "storage error")
        }
    }
}

async fn download(State(app): State<Arc<App>>, AxPath((owner, name, oid)): AxPath<(String, String, String)>, headers: HeaderMap) -> Response {
    if !valid_oid(&oid) {
        return err(StatusCode::BAD_REQUEST, "invalid oid");
    }
    let repo = match authorize(&app, &headers, &owner, &name, Access::Read).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let path = lfs_path(&app, &repo, &oid);
    let Ok(f) = tokio::fs::File::open(&path).await else {
        return err(StatusCode::NOT_FOUND, "object not found");
    };
    let mut res = Body::from_stream(ReaderStream::with_capacity(f, 64 * 1024)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    res
}

/// Clients optionally POST here after upload; 200 means "stored".
async fn verify(State(app): State<Arc<App>>, AxPath((owner, name, oid)): AxPath<(String, String, String)>, headers: HeaderMap) -> Response {
    if !valid_oid(&oid) {
        return err(StatusCode::BAD_REQUEST, "invalid oid");
    }
    let repo = match authorize(&app, &headers, &owner, &name, Access::Write).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if lfs_path(&app, &repo, &oid).exists() {
        StatusCode::OK.into_response()
    } else {
        err(StatusCode::NOT_FOUND, "object not found")
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/info/lfs/objects/batch", post(batch))
        .route("/{owner}/{name}/info/lfs/objects/{oid}", get(download).put(upload).post(verify))
        .route("/{owner}/{name}/info/lfs/objects/{oid}/verify", post(verify))
        // Some clients build URLs without the .git suffix differently; accept both.
        .route("/{owner}/{name}/lfs/objects/batch", post(batch))
        .route("/{owner}/{name}/lfs/objects/{oid}", get(download).put(upload).post(verify))
}

/// Objects referenced by LFS pointers in any commit reachable from any ref.
fn referenced_oids(dir: &std::path::Path) -> std::collections::HashSet<String> {
    use std::io::Write;
    let mut keep = std::collections::HashSet::new();
    // Feed stdin from a thread: writing a multi-megabyte object list while
    // the child is also writing its answer deadlocks on full pipe buffers.
    let run = |args: &[&str], stdin: Option<&[u8]>| -> Option<Vec<u8>> {
        let mut c = std::process::Command::new("git");
        c.arg("--git-dir").arg(dir).args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).env_remove("GIT_DIR");
        let mut child = c.spawn().ok()?;
        let mut pipe = child.stdin.take()?;
        let data = stdin.map(|d| d.to_vec());
        let writer = std::thread::spawn(move || {
            let _ = pipe.write_all(&data.unwrap_or_default());
        });
        let out = child.wait_with_output().ok()?;
        let _ = writer.join();
        out.status.success().then_some(out.stdout)
    };
    let Some(objects) = run(&["rev-list", "--objects", "--all"], None) else { return keep };
    let shas: Vec<&[u8]> = objects.split(|b| *b == b'\n').filter_map(|l| l.split(|b| *b == b' ').next().filter(|s| s.len() == 40)).collect();
    if shas.is_empty() {
        return keep;
    }
    // Sizes first: LFS pointers are tiny, so only small blobs are worth reading.
    let mut list = Vec::with_capacity(shas.len() * 41);
    for s in &shas {
        list.extend_from_slice(s);
        list.push(b'\n');
    }
    let Some(meta) = run(&["cat-file", "--batch-check=%(objectname) %(objecttype) %(objectsize)"], Some(&list)) else { return keep };
    let mut small = Vec::new();
    for line in meta.split(|b| *b == b'\n') {
        let f: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
        if f.len() == 3 && f[1] == b"blob" {
            if let Ok(size) = std::str::from_utf8(f[2]).unwrap_or("").parse::<u64>() {
                if size < 4096 {
                    small.extend_from_slice(f[0]);
                    small.push(b'\n');
                }
            }
        }
    }
    if small.is_empty() {
        return keep;
    }
    let Some(contents) = run(&["cat-file", "--batch"], Some(&small)) else { return keep };
    let text = String::from_utf8_lossy(&contents);
    for line in text.lines() {
        if let Some(oid) = line.trim_end().strip_prefix("oid sha256:") {
            if valid_oid(oid) {
                keep.insert(oid.to_owned());
            }
        }
    }
    keep
}

/// Delete LFS objects no longer referenced by any ref. Returns (removed, bytes).
pub fn gc(app: &App, dry_run: bool, only: Option<&str>) -> Result<(usize, u64), String> {
    let admin = crate::db::User { id: 0, name: String::new(), email: String::new(), is_admin: true, unread: 0 };
    let repos = app.db.visible_repos(Some(&admin), None).map_err(|e| e.to_string())?;
    let (mut removed, mut bytes) = (0, 0u64);
    // Objects of repositories that no longer exist (their ids may be reused).
    let live: std::collections::HashSet<String> = repos.iter().map(|r| r.id.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(app.data.join("lfs")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.bytes().all(|b| b.is_ascii_digit()) && !live.contains(&name) {
                if dry_run {
                    println!("would remove objects of deleted repo id {name}");
                } else {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }
    for repo in repos {
        let full = format!("{}/{}", repo.owner, repo.name);
        if only.is_some_and(|o| o != full) {
            continue;
        }
        eprintln!("scanning {full}");
        let keep = referenced_oids(&app.repo_dir(&repo));
        let root = app.data.join("lfs").join(repo.id.to_string());
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if valid_oid(name) && !keep.contains(name) {
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        if dry_run {
                            println!("would remove {full} {name} ({size} bytes)");
                            removed += 1;
                            bytes += size;
                        } else if std::fs::remove_file(&p).is_ok() {
                            removed += 1;
                            bytes += size;
                        }
                    }
                }
            }
        }
    }
    Ok((removed, bytes))
}
