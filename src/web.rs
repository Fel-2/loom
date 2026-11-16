//! HTTP routes: browsing, auth, settings, repo creation, git smart HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{html, Markup};
use serde::Deserialize;

use crate::app::{App, Login};
use crate::db::{self, Access, Repo, User};
use crate::{git, githttp, ui};

type St = State<Arc<App>>;

const CSS: &str = include_str!("../static/style.css");
const COOKIE: &str = "loom_session";

// ---------- errors ----------

pub enum Err {
    NotFound,
    Internal(String),
}

impl From<git::Error> for Err {
    fn from(e: git::Error) -> Self {
        match e {
            git::Error::NotFound => Err::NotFound,
            git::Error::Git(m) => Err::Internal(m),
        }
    }
}

impl From<db::Error> for Err {
    fn from(e: db::Error) -> Self {
        Err::Internal(e.to_string())
    }
}

impl IntoResponse for Err {
    fn into_response(self) -> Response {
        match self {
            Err::NotFound => (StatusCode::NOT_FOUND, ui::not_found()).into_response(),
            Err::Internal(m) => {
                eprintln!("error: {m}");
                (StatusCode::INTERNAL_SERVER_ERROR, ui::error("internal error")).into_response()
            }
        }
    }
}

type Res = Result<Response, Err>;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, Err> + Send + 'static) -> Result<T, Err> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Err::Internal(e.to_string()))?
}

// ---------- viewer ----------

/// The signed-in user, if any (from the session cookie).
pub struct Viewer(pub Option<User>);

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
}

impl FromRequestParts<Arc<App>> for Viewer {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let user = cookie(&parts.headers, COOKIE).and_then(|raw| app.db.session_user(raw).ok().flatten());
        Ok(Viewer(user))
    }
}

/// Signed-in user, or a redirect to the login page.
pub struct Authed(pub User);

impl FromRequestParts<Arc<App>> for Authed {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let Viewer(u) = Viewer::from_request_parts(parts, app).await.unwrap_or(Viewer(None));
        u.map(Authed).ok_or_else(|| {
            let next = parts.uri.path_and_query().map_or("/", |p| p.as_str());
            Redirect::to(&format!("/login?next={}", urlencode(next))).into_response()
        })
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Reject cross-site form posts: if the browser sends Origin, it must match Host.
/// (Cookies are also SameSite=Lax.) Git clients send no Origin, so they pass.
async fn same_origin(req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let h = req.headers();
        let host = h.get(header::HOST).and_then(|v| v.to_str().ok());
        if let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let origin_host = origin.split_once("://").map(|(_, r)| r);
            if origin == "null" || origin_host != host {
                return (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response();
            }
        }
    }
    next.run(req).await
}

// ---------- caching ----------

/// Conditional response keyed on immutable git ids. HTML varies by viewer, so
/// the tag includes the user id and caching is `private`. `pinned` = the URL
/// names a full sha, so the content can never change.
fn etagged(headers: &HeaderMap, viewer: Option<&User>, tag: String, pinned: bool, body: impl FnOnce() -> Response) -> Response {
    let tag = format!("\"{tag}-u{}\"", viewer.map_or(0, |u| u.id));
    let cache = if pinned { "private, max-age=31536000, immutable" } else { "private, no-cache" };
    let hit = headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes());
    let mut res = if hit { StatusCode::NOT_MODIFIED.into_response() } else { body() };
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------- repo context ----------

struct Ctx {
    viewer: Option<User>,
    repo: Repo,
    dir: PathBuf,
    http: String,
    ssh: Option<String>,
}

impl Ctx {
    fn load(app: &Arc<App>, viewer: Option<User>, owner: &str, name: &str) -> Result<Ctx, Err> {
        let (repo, _) = app.authorize(viewer.as_ref(), owner, name, Access::Read).ok_or(Err::NotFound)?;
        let dir = app.repo_dir(&repo);
        let (http, ssh) = app.clone_urls(&repo);
        Ok(Ctx { viewer, repo, dir, http, ssh })
    }
    fn base(&self) -> String {
        format!("/{}/{}", self.repo.owner, self.repo.name)
    }
    fn clone(&self) -> ui::Clone<'_> {
        ui::Clone { http: &self.http, ssh: self.ssh.as_deref() }
    }
    fn header(&self, tab: ui::Tab, rev: &str) -> Markup {
        ui::repo_header(&self.repo, &self.clone(), tab, rev)
    }
    fn page(&self, title: &str, body: Markup) -> Response {
        let t = format!("{title}{}{}/{}", if title.is_empty() { "" } else { " · " }, self.repo.owner, self.repo.name);
        ui::page(&t, self.viewer.as_ref(), body).into_response()
    }
    fn open(&self) -> Result<gix::Repository, Err> {
        Ok(git::open(&self.dir)?)
    }
}

// ---------- browse ----------

async fn index(State(app): St, Viewer(v): Viewer) -> Res {
    let repos = app.db.visible_repos(v.as_ref(), None)?;
    Ok(ui::page("repositories", v.as_ref(), ui::index(&repos, None)).into_response())
}

async fn owner(State(app): St, Viewer(v): Viewer, Path(owner): Path<String>) -> Res {
    let repos = app.db.visible_repos(v.as_ref(), Some(&owner))?;
    if repos.is_empty() && app.db.user_by_name(&owner)?.is_none() {
        return Err(Err::NotFound);
    }
    Ok(ui::page(&owner, v.as_ref(), ui::index(&repos, Some(&owner))).into_response())
}

async fn overview(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let Some(head) = git::head_commit(&repo) else {
            let body = html! { (ctx.header(ui::Tab::Overview, "HEAD")) (ui::empty_repo(&ctx.clone())) };
            return Ok(ctx.page("", body));
        };
        let rev = git::default_branch(&repo).unwrap_or_else(|| head.to_string());
        Ok(etagged(&headers, ctx.viewer.as_ref(), format!("o-{head}"), false, || {
            let base = ctx.base();
            let entries = git::tree(&repo, head, "").unwrap_or_default();
            let commits = git::log(&repo, head, "", 0, 8).unwrap_or_default();
            let readme = git::readme(&repo, head, "");
            let body = html! {
                (ctx.header(ui::Tab::Overview, &rev))
                div.split {
                    div.stack {
                        (ui::crumbs(&base, &rev, ""))
                        (ui::tree(&base, &rev, "", &entries))
                        @if let Some((n, c)) = &readme { (ui::readme(n, c)) }
                    }
                    aside.card.flush {
                        div.card-head { "recent commits" span.grow {} a href={ (base) "/log/" (rev) } { "all" } }
                        (ui::commit_rows(&base, &commits))
                    }
                }
            };
            ctx.page("", body)
        }))
    })
    .await
}

async fn tree(State(app): St, Viewer(v): Viewer, Path((owner, name, rest)): Path<(String, String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let (id, rev, path) = git::split_rev(&repo, &rest)?;
        let base = ctx.base();
        if let git::PathKind::Blob = git::path_kind(&repo, id, path)? {
            return Ok(Redirect::to(&format!("{base}/blob/{rev}/{path}")).into_response());
        }
        let entries = git::tree(&repo, id, path)?;
        Ok(etagged(&headers, ctx.viewer.as_ref(), format!("t-{id}-{path}"), is_full_sha(rev), || {
            let readme = git::readme(&repo, id, path);
            let body = html! {
                (ctx.header(ui::Tab::Overview, rev))
                div.stack {
                    (ui::crumbs(&base, rev, path))
                    (ui::tree(&base, rev, path, &entries))
                    @if let Some((n, c)) = &readme { (ui::readme(n, c)) }
                }
            };
            ctx.page(path, body)
        }))
    })
    .await
}

async fn blob(State(app): St, Viewer(v): Viewer, Path((owner, name, rest)): Path<(String, String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let (id, rev, path) = git::split_rev(&repo, &rest)?;
        let (bid, data) = git::blob(&repo, id, path)?;
        Ok(etagged(&headers, ctx.viewer.as_ref(), format!("b-{bid}-{rev}"), is_full_sha(rev), || {
            let base = ctx.base();
            let body = html! {
                (ctx.header(ui::Tab::Overview, rev))
                div.stack {
                    (ui::crumbs(&base, rev, path))
                    (ui::blob(&base, rev, path, &data))
                }
            };
            ctx.page(path, body)
        }))
    })
    .await
}

async fn raw(State(app): St, Viewer(v): Viewer, Path((owner, name, rest)): Path<(String, String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let (id, rev, path) = git::split_rev(&repo, &rest)?;
        let (bid, data) = git::blob(&repo, id, path)?;
        let tag = format!("\"{bid}\"");
        let cache = match (ctx.repo.private, is_full_sha(rev)) {
            (false, true) => "public, max-age=31536000, immutable",
            (false, false) => "public, no-cache",
            (true, _) => "private, no-cache",
        };
        let hit = headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes());
        let mut res = if hit {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            // Never let repo content run as HTML.
            let ct = if std::str::from_utf8(&data).is_ok() { "text/plain; charset=utf-8" } else { "application/octet-stream" };
            ([(header::CONTENT_TYPE, ct)], data).into_response()
        };
        let h = res.headers_mut();
        h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        if let Ok(v) = HeaderValue::from_str(&tag) {
            h.insert(header::ETAG, v);
        }
        Ok(res)
    })
    .await
}

#[derive(Deserialize)]
struct LogQ {
    path: Option<String>,
    page: Option<usize>,
}

async fn log(
    State(app): St,
    Viewer(v): Viewer,
    Path((owner, name, rev)): Path<(String, String, String)>,
    Query(q): Query<LogQ>,
    headers: HeaderMap,
) -> Res {
    const PER: usize = 50;
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let id = git::resolve_commit(&repo, &rev)?;
        let path = q.path.unwrap_or_default();
        let page = q.page.unwrap_or(0);
        Ok(etagged(&headers, ctx.viewer.as_ref(), format!("l-{id}-{path}-{page}"), is_full_sha(&rev), || {
            let commits = git::log(&repo, id, &path, page * PER, PER + 1).unwrap_or_default();
            let more = commits.len() > PER;
            let base = ctx.base();
            let qp = if path.is_empty() { String::new() } else { format!("path={}&", urlencode(&path)) };
            let body = html! {
                (ctx.header(ui::Tab::Commits, &rev))
                @if !path.is_empty() { p.muted { "history of " code { (path) } } }
                div.card.flush { (ui::commit_rows(&base, &commits[..commits.len().min(PER)])) }
                nav.pager {
                    @if page > 0 { a href={ "?" (qp) "page=" (page - 1) } { "← newer" } }
                    @if more { a href={ "?" (qp) "page=" (page + 1) } { "older →" } }
                }
            };
            ctx.page("commits", body)
        }))
    })
    .await
}

async fn commit(State(app): St, Viewer(v): Viewer, Path((owner, name, rev)): Path<(String, String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let id = git::resolve_commit(&repo, &rev)?;
        Ok(etagged(&headers, ctx.viewer.as_ref(), format!("c-{id}"), is_full_sha(&rev), || {
            let built = (|| -> Result<_, git::Error> { Ok((git::commit(&repo, id)?, git::commit_diff(&repo, id)?)) })();
            match built {
                Ok((c, files)) => {
                    let body = html! { (ctx.header(ui::Tab::Commits, &rev)) (ui::commit(&ctx.base(), &c, &files)) };
                    ctx.page(&c.summary, body)
                }
                Err(e) => Err::from(e).into_response(),
            }
        }))
    })
    .await
}

async fn refs(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let (branches, tags) = git::refs(&repo)?;
        let default = git::default_branch(&repo);
        let rev = default.clone().unwrap_or_else(|| "HEAD".into());
        let body = html! {
            (ctx.header(ui::Tab::Refs, &rev))
            (ui::refs(&ctx.base(), &branches, &tags, default.as_deref()))
        };
        Ok(ctx.page("refs", body))
    })
    .await
}

async fn css(headers: HeaderMap) -> Response {
    let tag = format!("\"css-{:x}\"", CSS.len());
    let hit = headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes());
    let mut res = if hit { StatusCode::NOT_MODIFIED.into_response() } else { ([(header::CONTENT_TYPE, "text/css")], CSS).into_response() };
    if let Ok(v) = HeaderValue::from_str(&tag) {
        res.headers_mut().insert(header::ETAG, v);
    }
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, no-cache"));
    res
}

// ---------- auth ----------

#[derive(Deserialize)]
struct NextQ {
    next: Option<String>,
}

/// Only same-site relative paths: no `//host` or `/\host` open redirects.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") => n.to_owned(),
        _ => "/".into(),
    }
}

async fn login_page(Viewer(v): Viewer, Query(q): Query<NextQ>) -> Response {
    let next = safe_next(q.next.as_deref());
    if v.is_some() {
        return Redirect::to(&next).into_response();
    }
    ui::page("sign in", None, ui::login(&next, None)).into_response()
}

#[derive(Deserialize)]
struct LoginF {
    name: String,
    password: String,
    next: Option<String>,
}

fn session_cookie(app: &App, value: &str, max_age: i64) -> HeaderValue {
    let secure = if app.secure_cookies { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie header")
}

async fn login(State(app): St, Form(f): Form<LoginF>) -> Res {
    let next = safe_next(f.next.as_deref());
    let a = app.clone();
    let name = f.name.trim().to_owned();
    let result = blocking(move || Ok(a.login(&name, &f.password))).await?;
    let err = match result {
        Login::Ok(user) => {
            let raw = app.db.create_session(user.id)?;
            let mut res = Redirect::to(&next).into_response();
            res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, &raw, db::SESSION_DAYS * 86400));
            return Ok(res);
        }
        Login::Bad => "Wrong user name or password.",
        Login::Throttled => "Too many failed attempts. Try again in 15 minutes.",
    };
    Ok((StatusCode::UNAUTHORIZED, ui::page("sign in", None, ui::login(&next, Some(err)))).into_response())
}

async fn logout(State(app): St, headers: HeaderMap) -> Res {
    if let Some(raw) = cookie(&headers, COOKIE) {
        app.db.delete_session(raw)?;
    }
    let mut res = Redirect::to("/").into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, "", 0));
    Ok(res)
}

// ---------- settings ----------

fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

async fn keys_page(State(app): St, Authed(u): Authed) -> Res {
    let keys = app.db.keys(u.id)?;
    Ok(ui::page("ssh keys", Some(&u), ui::keys(&keys, None)).into_response())
}

#[derive(Deserialize)]
struct KeyF {
    title: String,
    key: String,
}

async fn keys_add(State(app): St, Authed(u): Authed, Form(f): Form<KeyF>) -> Res {
    let err = match crate::cli::parse_pubkey(&f.key) {
        Ok((key, fp)) => {
            let title = match f.title.trim() {
                "" => f.key.split_whitespace().nth(2).unwrap_or("key").to_owned(),
                t => t.to_owned(),
            };
            match app.db.add_key(u.id, &title, &fp, &key) {
                Ok(()) => return Ok(see_other("/settings/keys")),
                Err(db::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                    "That key is already registered.".to_owned()
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => e,
    };
    let keys = app.db.keys(u.id)?;
    Ok((StatusCode::BAD_REQUEST, ui::page("ssh keys", Some(&u), ui::keys(&keys, Some(&err)))).into_response())
}

async fn keys_delete(State(app): St, Authed(u): Authed, Path(id): Path<i64>) -> Res {
    app.db.delete_key(u.id, id)?;
    Ok(see_other("/settings/keys"))
}

async fn tokens_page(State(app): St, Authed(u): Authed) -> Res {
    let tokens = app.db.tokens(u.id)?;
    Ok(ui::page("access tokens", Some(&u), ui::tokens(&tokens, None, None)).into_response())
}

#[derive(Deserialize)]
struct TokenF {
    title: String,
}

async fn tokens_add(State(app): St, Authed(u): Authed, Form(f): Form<TokenF>) -> Res {
    let title = f.title.trim();
    if title.is_empty() || title.len() > 100 {
        let tokens = app.db.tokens(u.id)?;
        let page = ui::tokens(&tokens, None, Some("Give the token a title (max 100 characters)."));
        return Ok((StatusCode::BAD_REQUEST, ui::page("access tokens", Some(&u), page)).into_response());
    }
    let raw = app.db.create_token(u.id, title)?;
    let tokens = app.db.tokens(u.id)?;
    let mut res = ui::page("access tokens", Some(&u), ui::tokens(&tokens, Some(&raw), None)).into_response();
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}

async fn tokens_delete(State(app): St, Authed(u): Authed, Path(id): Path<i64>) -> Res {
    app.db.delete_token(u.id, id)?;
    Ok(see_other("/settings/tokens"))
}

async fn password_page(Authed(u): Authed) -> Response {
    ui::page("password", Some(&u), ui::password(None, None)).into_response()
}

#[derive(Deserialize)]
struct PasswordF {
    current: String,
    new: String,
}

async fn password_change(State(app): St, Authed(u): Authed, Form(f): Form<PasswordF>) -> Res {
    let fail = |msg: &str| Ok((StatusCode::BAD_REQUEST, ui::page("password", Some(&u), ui::password(None, Some(msg)))).into_response());
    if f.new.chars().count() < 8 {
        return fail("The new password must be at least 8 characters.");
    }
    let (a, name) = (app.clone(), u.name.clone());
    let current = f.current;
    match blocking(move || Ok(a.login(&name, &current))).await? {
        Login::Ok(_) => {}
        Login::Bad => return fail("Current password is wrong."),
        Login::Throttled => return fail("Too many failed attempts. Try again in 15 minutes."),
    }
    let (a, id, new) = (app.clone(), u.id, f.new);
    blocking(move || Ok(a.db.set_password(id, &new)?)).await?;
    // All sessions were revoked; start a fresh one for this browser.
    let raw = app.db.create_session(u.id)?;
    let mut res = ui::page("password", Some(&u), ui::password(Some("Password changed. Other sessions were signed out."), None)).into_response();
    res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, &raw, db::SESSION_DAYS * 86400));
    Ok(res)
}

// ---------- new repo ----------

async fn new_page(Authed(u): Authed) -> Response {
    ui::page("new repository", Some(&u), ui::new_repo(&u.name, None)).into_response()
}

#[derive(Deserialize)]
struct NewF {
    name: String,
    description: String,
    private: Option<String>,
}

async fn new_create(State(app): St, Authed(u): Authed, Form(f): Form<NewF>) -> Res {
    let (a, user) = (app.clone(), u.clone());
    let res = blocking(move || {
        Ok(a.create_repo(Some(&user), &user.name, f.name.trim(), &f.description, f.private.is_some()))
    })
    .await?;
    match res {
        Ok(r) => Ok(see_other(&format!("/{}/{}", r.owner, r.name))),
        Err(e) => Ok((StatusCode::BAD_REQUEST, ui::page("new repository", Some(&u), ui::new_repo(&u.name, Some(&e)))).into_response()),
    }
}

async fn fallback() -> Response {
    (StatusCode::NOT_FOUND, ui::not_found()).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(css))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/new", get(new_page).post(new_create))
        .route("/settings", get(|| async { Redirect::to("/settings/keys") }))
        .route("/settings/keys", get(keys_page).post(keys_add))
        .route("/settings/keys/{id}/delete", post(keys_delete))
        .route("/settings/tokens", get(tokens_page).post(tokens_add))
        .route("/settings/tokens/{id}/delete", post(tokens_delete))
        .route("/settings/password", get(password_page).post(password_change))
        .route("/{owner}", get(owner))
        .route("/{owner}/{name}", get(overview))
        .route("/{owner}/{name}/info/refs", get(githttp::info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(githttp::upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(githttp::receive_pack))
        .route("/{owner}/{name}/tree/{*rest}", get(tree))
        .route("/{owner}/{name}/blob/{*rest}", get(blob))
        .route("/{owner}/{name}/raw/{*rest}", get(raw))
        .route("/{owner}/{name}/log/{*rev}", get(log))
        .route("/{owner}/{name}/commit/{rev}", get(commit))
        .route("/{owner}/{name}/refs", get(refs))
        .fallback(fallback)
        .layer(middleware::from_fn(same_origin))
        .with_state(app)
}
