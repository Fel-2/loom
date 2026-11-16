//! Git smart HTTP: `info/refs`, `git-upload-pack`, `git-receive-pack`.
//! Bodies stream straight between the socket and a `git` child process.

use std::io::Read;
use std::process::Stdio;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::app::App;
use crate::db::{Access, User};
use crate::git;

/// Gzipped request bodies are only used for fetch negotiation; cap them.
const MAX_GZIP_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Service {
    Upload,
    Receive,
}

impl Service {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "git-upload-pack" => Some(Service::Upload),
            "git-receive-pack" => Some(Service::Receive),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Service::Upload => "upload-pack",
            Service::Receive => "receive-pack",
        }
    }
    fn need(self) -> Access {
        match self {
            Service::Upload => Access::Read,
            Service::Receive => Access::Write,
        }
    }
}

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"loom\", charset=\"UTF-8\"")],
        "authentication required\n",
    )
        .into_response()
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "repository not found\n").into_response()
}

/// HTTP Basic: username + password, or any username + personal access token.
pub(crate) async fn basic_user(app: &Arc<App>, headers: &HeaderMap) -> Option<User> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let b64 = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let raw = String::from_utf8(raw).ok()?;
    let (user, secret) = raw.split_once(':')?;
    if secret.starts_with("loom_") {
        return app.db.token_user(secret).ok().flatten();
    }
    let (app, user, secret) = (app.clone(), user.to_owned(), secret.to_owned());
    tokio::task::spawn_blocking(move || match app.login(&user, &secret, "git-http") {
        crate::app::Login::Ok(u) => Some(u),
        _ => None,
    })
    .await
    .ok()
    .flatten()
}

/// Resolve the repo and check access, challenging anonymous clients first.
async fn authorize(
    app: &Arc<App>,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    svc: Service,
) -> Result<(crate::db::Repo, Option<User>), Response> {
    let user = basic_user(app, headers).await;
    if headers.contains_key(header::AUTHORIZATION) && user.is_none() {
        return Err(challenge());
    }
    match app.authorize(user.as_ref(), owner, name, svc.need()) {
        Some((repo, _)) => Ok((repo, user)),
        None if user.is_none() => Err(challenge()),
        None => Err(not_found()),
    }
}

/// `Git-Protocol` header, passed to git as `GIT_PROTOCOL` when well-formed.
fn git_protocol(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("git-protocol")?.to_str().ok()?;
    let ok = v.len() < 256 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"=:._-".contains(&b));
    ok.then(|| v.to_owned())
}

fn pkt_line(s: &str) -> Vec<u8> {
    format!("{:04x}{s}", s.len() + 4).into_bytes()
}

fn no_cache(res: &mut Response, content_type: String) {
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=0, must-revalidate"));
}

#[derive(Deserialize)]
pub struct InfoQ {
    service: Option<String>,
}

pub async fn info_refs(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<InfoQ>,
    headers: HeaderMap,
) -> Response {
    // Dumb HTTP is not supported.
    let Some(svc) = q.service.as_deref().and_then(Service::parse) else {
        return (StatusCode::FORBIDDEN, "smart HTTP only\n").into_response();
    };
    let dir = match authorize(&app, &headers, &owner, &name, svc).await {
        Ok((repo, _)) => app.repo_dir(&repo),
        Err(r) => return r,
    };
    let proto = git_protocol(&headers);
    let mut cmd = Command::new("git");
    // loom's scratch refs (fork compares) are never served.
    cmd.args(["-c", "uploadpack.hideRefs=refs/loom"]);
    cmd.args([svc.name(), "--stateless-rpc", "--http-backend-info-refs"]).arg(&dir);
    if let Some(p) = &proto {
        cmd.env("GIT_PROTOCOL", p);
    }
    let _permit = match app.git_slots.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return internal("git slot closed"),
    };
    let out = match cmd.stdin(Stdio::null()).stderr(Stdio::inherit()).output().await {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => return internal(&format!("git {} exited {}", svc.name(), o.status)),
        Err(e) => return internal(&format!("spawn git: {e}")),
    };
    // Protocol v2 fetch skips the service banner, like git-http-backend.
    let v2 = svc == Service::Upload && proto.as_deref().is_some_and(|p| p.contains("version=2"));
    let mut body = Vec::with_capacity(out.len() + 64);
    if !v2 {
        body.extend(pkt_line(&format!("# service=git-{}\n", svc.name())));
        body.extend(b"0000");
    }
    body.extend(out);
    let mut res = body.into_response();
    no_cache(&mut res, format!("application/x-git-{}-advertisement", svc.name()));
    res
}

pub async fn upload_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(app, owner, name, Service::Upload, headers, body).await
}

pub async fn receive_pack(
    State(app): State<Arc<App>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(app, owner, name, Service::Receive, headers, body).await
}

async fn rpc(app: Arc<App>, owner: String, name: String, svc: Service, headers: HeaderMap, body: Body) -> Response {
    let (repo, user) = match authorize(&app, &headers, &owner, &name, svc).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let dir = app.repo_dir(&repo);
    let gzip = headers.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));

    let mut cmd = Command::new("git");
    if svc == Service::Receive {
        app.receive_pack_setup(&mut cmd, &repo, user.as_ref());
    } else {
        cmd.args(["-c", "uploadpack.hideRefs=refs/loom"]);
    }
    cmd.args([svc.name(), "--stateless-rpc"]).arg(&dir);
    if let Some(p) = git_protocol(&headers) {
        cmd.env("GIT_PROTOCOL", p);
    }
    // At most LOOM_MAX_GIT streaming git processes; the permit is dropped when
    // the child exits (in the reaper task below).
    let Ok(permit) = app.git_slots.clone().acquire_owned().await else {
        return internal("git slot closed");
    };
    let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn() {
        Ok(c) => c,
        Err(e) => return internal(&format!("spawn git: {e}")),
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return internal("git pipes missing");
    };

    // Feed the request body to git, decompressing small gzipped bodies in memory.
    tokio::spawn(async move {
        let mut stream = body.into_data_stream();
        if gzip {
            let mut buf = Vec::new();
            while let Some(Ok(chunk)) = stream.next().await {
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_GZIP_BODY {
                    return;
                }
            }
            let mut plain = Vec::new();
            if flate2::read::GzDecoder::new(&buf[..]).take(MAX_GZIP_BODY as u64).read_to_end(&mut plain).is_err() {
                return;
            }
            let _ = stdin.write_all(&plain).await;
        } else {
            while let Some(Ok(chunk)) = stream.next().await {
                if stdin.write_all(&chunk).await.is_err() {
                    return;
                }
            }
        }
        // Dropping stdin closes it, letting git finish.
    });

    // Reap the child; after a push, make sure HEAD points somewhere real.
    tokio::spawn(async move {
        let ok = child.wait().await.is_ok_and(|s| s.success());
        drop(permit);
        if ok && svc == Service::Receive {
            let _ = tokio::task::spawn_blocking(move || git::fix_head(&dir)).await;
        }
    });

    let mut res = Body::from_stream(ReaderStream::with_capacity(stdout, 64 * 1024)).into_response();
    no_cache(&mut res, format!("application/x-git-{}-result", svc.name()));
    res
}

fn internal(msg: &str) -> Response {
    eprintln!("githttp: {msg}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
}
