//! HTTP routes: browsing, auth, settings, repo creation, git smart HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
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
use crate::{git, githttp, totp, ui};

pub(crate) type St = State<Arc<App>>;

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

pub(crate) type Res = Result<Response, Err>;

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, Err> + Send + 'static) -> Result<T, Err> {
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
        let mut user = cookie(&parts.headers, COOKIE).and_then(|raw| app.db.session_user(raw).ok().flatten());
        if let Some(u) = &mut user {
            u.unread = app.db.unread_count(u.id).unwrap_or(0);
        }
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

pub(crate) fn urlencode(s: &str) -> String {
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

/// Every page ships no scripts, so deny everything and allow only what the
/// UI uses: the stylesheet, forms, inline `style` attributes (label colours,
/// bar widths) — and images from anywhere, because READMEs and issues embed
/// badges and screenshots hosted elsewhere. Images cannot execute script, so
/// the cost is third-party requests, not XSS.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self' http: https: data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    res
}

// ---------- caching ----------

/// Conditional response. The tag = git ids + viewer + repo header state, so a
/// 304 is only sent when the page would be byte-identical. HTML always
/// revalidates (the header shows live stars/forks), which costs one cheap 304.
fn etagged(headers: &HeaderMap, viewer: Option<&User>, tag: String, body: impl FnOnce() -> Response) -> Response {
    let tag = format!("\"{tag}-u{}\"", viewer.map_or(0, |u| u.id));
    let cache = "private, no-cache";
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

pub(crate) struct Ctx {
    pub viewer: Option<User>,
    pub repo: Repo,
    pub access: Access,
    pub dir: PathBuf,
    http: String,
    ssh: Option<String>,
    pub meta: ui::Meta,
}

impl Ctx {
    pub fn load(app: &Arc<App>, viewer: Option<User>, owner: &str, name: &str) -> Result<Ctx, Err> {
        Self::load_with(app, viewer, owner, name, Access::Read)
    }

    /// Like `load`, but 404s unless the viewer has `need`.
    pub fn load_with(app: &Arc<App>, viewer: Option<User>, owner: &str, name: &str, need: Access) -> Result<Ctx, Err> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let repo = app.db.repo(owner, name)?.ok_or(Err::NotFound)?;
        let access = app.db.access(viewer.as_ref(), &repo)?;
        if access < need {
            return Err(Err::NotFound);
        }
        let dir = app.repo_dir(&repo);
        let (http, ssh) = app.clone_urls(&repo);
        let parent = match repo.fork_of {
            Some(id) => app.db.repo_by_id(id)?.and_then(|p| {
                // Don't reveal a private parent to someone who can't see it.
                let visible = app.db.access(viewer.as_ref(), &p).is_ok_and(|a| a >= Access::Read);
                visible.then(|| format!("{}/{}", p.owner, p.name))
            }),
            None => None,
        };
        let meta = ui::Meta {
            stars: app.db.star_count(repo.id)?,
            starred: match &viewer {
                Some(u) => Some(app.db.starred(u.id, repo.id)?),
                None => None,
            },
            forks: app.db.fork_count(repo.id)?,
            parent,
            can_admin: access >= Access::Admin,
            open_issues: app.db.open_count(repo.id, false)?,
            open_pulls: app.db.open_count(repo.id, true)?,
            watching: match &viewer {
                Some(u) => Some(app.db.watching(u.id, repo.id)?),
                None => None,
            },
        };
        Ok(Ctx { viewer, repo, access, dir, http, ssh, meta })
    }

    /// Hash of everything in the repo header that isn't a git id.
    fn version(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&self.repo.owner, &self.repo.name, &self.repo.description, self.repo.private, self.repo.archived).hash(&mut h);
        (self.meta.stars, self.meta.starred, self.meta.forks, &self.meta.parent, self.meta.can_admin, self.meta.open_issues, self.meta.open_pulls, self.meta.watching).hash(&mut h);
        h.finish()
    }

    fn etag(&self, headers: &HeaderMap, tag: String, body: impl FnOnce() -> Response) -> Response {
        etagged(headers, self.viewer.as_ref(), format!("{tag}-{:x}", self.version()), body)
    }
    pub fn base(&self) -> String {
        format!("/{}/{}", self.repo.owner, self.repo.name)
    }
    fn clone(&self) -> ui::Clone<'_> {
        ui::Clone { http: &self.http, ssh: self.ssh.as_deref() }
    }
    pub fn header(&self, tab: ui::Tab, rev: &str) -> Markup {
        ui::repo_header(&self.repo, &self.clone(), &self.meta, tab, rev)
    }
    pub fn page(&self, title: &str, body: Markup) -> Response {
        let t = format!("{title}{}{}/{}", if title.is_empty() { "" } else { " · " }, self.repo.owner, self.repo.name);
        let base = self.base();
        let desc: String = if self.repo.description.is_empty() { format!("{}/{} on loom", self.repo.owner, self.repo.name) } else { self.repo.description.clone() };
        let head = html! {
            link rel="alternate" type="application/atom+xml" title="commits" href={ (base) "/atom.xml" };
            link rel="alternate" type="application/atom+xml" title="issues" href={ (base) "/issues.atom.xml" };
            link rel="alternate" type="application/atom+xml" title="releases" href={ (base) "/releases.atom.xml" };
            meta property="og:title" content={ (self.repo.owner) "/" (self.repo.name) };
            meta property="og:description" content=(desc);
        };
        ui::page_head(&t, self.viewer.as_ref(), body, head).into_response()
    }
    pub fn open(&self) -> Result<gix::Repository, Err> {
        Ok(git::open(&self.dir)?)
    }
}

/// Last-commit info per entry, only when the repo opted in.
fn last_map(ctx: &Ctx, commit: gix::ObjectId, dir: &str, entries: &[git::Entry]) -> std::collections::HashMap<String, ui::LastCommit> {
    let mut m = std::collections::HashMap::new();
    if !ctx.repo.show_last_commit || entries.len() > 400 {
        return m;
    }
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    for e in entries {
        if let Some(l) = git::last_commit(&ctx.dir, &commit.to_string(), &format!("{prefix}{}", e.name)) {
            m.insert(e.name.clone(), ui::LastCommit { sha: l.sha, author: l.author, when: l.time });
        }
    }
    m
}

// ---------- browse ----------

async fn index(State(app): St, Viewer(v): Viewer) -> Res {
    let repos = app.db.visible_repos(v.as_ref(), None)?;
    if let Some(u) = &v {
        let feed = app.db.activity(Some(u), Some(u.id), None, 50)?;
        return Ok(ui::page("dashboard", Some(u), crate::ui_social::dashboard(&u.name, &feed, &repos)).into_response());
    }
    Ok(ui::page("repositories", v.as_ref(), ui::index(&repos, None)).into_response())
}

async fn owner(State(app): St, Viewer(v): Viewer, Path(owner): Path<String>) -> Res {
    let o = app.db.owner(&owner)?.ok_or(Err::NotFound)?;
    let repos = app.db.visible_repos(v.as_ref(), Some(&o.name))?;
    let (orgs, members) = if o.is_org { (Vec::new(), app.db.members(o.id)?) } else { (app.db.orgs_of(o.id)?, Vec::new()) };
    let can_admin = o.is_org && v.as_ref().is_some_and(|u| u.is_admin || app.db.org_role(&o.name, u.id).is_ok_and(|r| r == Access::Admin));
    let feed = app.db.activity(v.as_ref(), None, Some(&o.name), 30)?;
    let body = html! { (ui::owner_page(&o, &repos, &orgs, &members, can_admin)) div.mt { (crate::ui_social::activity(&feed)) } };
    Ok(ui::page(&o.name, v.as_ref(), body).into_response())
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
        Ok(ctx.etag(&headers, format!("o-{head}"), || {
            let base = ctx.base();
            let entries = git::tree(&repo, head, "").unwrap_or_default();
            let commits = git::log(&repo, head, "", 0, 8).unwrap_or_default();
            let readme = git::readme(&repo, head, "");
            let body = html! {
                (ctx.header(ui::Tab::Overview, &rev))
                div.split {
                    div.stack {
                        (ui::crumbs(&base, &rev, ""))
                        (ui::tree(&base, &rev, "", &entries, &last_map(&ctx, head, "", &entries)))
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
        Ok(ctx.etag(&headers, format!("t-{id}-{path}"), || {
            let readme = git::readme(&repo, id, path);
            let body = html! {
                (ctx.header(ui::Tab::Overview, rev))
                div.stack {
                    (ui::crumbs(&base, rev, path))
                    (ui::tree(&base, rev, path, &entries, &last_map(&ctx, id, path, &entries)))
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
        Ok(ctx.etag(&headers, format!("b-{bid}-{rev}"), || {
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

async fn commits_alias(
    State(app): St,
    Viewer(v): Viewer,
    Path((owner, name)): Path<(String, String)>,
    uri: axum::http::Uri,
) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let base = ctx.base();
    let rev = blocking(move || {
        let repo = ctx.open()?;
        git::default_branch(&repo).ok_or(Err::NotFound)
    })
    .await?;
    Ok(to_log(&base, &rev, &uri))
}

async fn commits_alias_rev(
    State(app): St,
    Viewer(v): Viewer,
    Path((owner, name, rev)): Path<(String, String, String)>,
    uri: axum::http::Uri,
) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    Ok(to_log(&ctx.base(), &rev, &uri))
}

fn to_log(base: &str, rev: &str, uri: &axum::http::Uri) -> Response {
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    Redirect::to(&format!("{base}/log/{}{query}", urlencode(rev))).into_response()
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
        Ok(ctx.etag(&headers, format!("l-{id}-{path}-{page}"), || {
            let commits = git::log(&repo, id, &path, page * PER, PER + 1).unwrap_or_default();
            let more = commits.len() > PER;
            let base = ctx.base();
            let qp = if path.is_empty() { String::new() } else { format!("path={}&", urlencode(&path)) };
            let body = html! {
                (ctx.header(ui::Tab::Commits, &rev))
                @if !path.is_empty() { p.muted { "history of " code { (path) } } }
                div.card.flush { (ui::commit_rows(&base, &commits[..commits.len().min(PER)])) }
                nav.pager { span.muted.small { "prefer a graph? " } a href={ (base) "/graph/" (rev) } { "graph view" } }
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
        let statuses = app.db.statuses(ctx.repo.id, &id.to_string())?;
        let stag: u64 = statuses.iter().map(|s| s.created as u64 ^ s.state.len() as u64).sum();
        Ok(ctx.etag(&headers, format!("c-{id}-{}-{stag}", statuses.len()), || {
            let built = (|| -> Result<_, git::Error> { Ok((git::commit(&repo, id)?, git::commit_diff(&repo, id)?)) })();
            match built {
                Ok((c, files)) => {
                    let body = html! {
                        (ctx.header(ui::Tab::Commits, &rev))
                        @if !statuses.is_empty() { div.card.mb { (crate::ui_social::statuses(&statuses, &[])) } }
                        (ui::commit(&ctx.base(), &c, &files))
                    };
                    ctx.page(&c.summary, body)
                }
                Err(e) => Err::from(e).into_response(),
            }
        }))
    })
    .await
}

async fn blame(State(app): St, Viewer(v): Viewer, Path((owner, name, rest)): Path<(String, String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let (id, rev, path) = git::split_rev(&repo, &rest)?;
        let (_, data) = git::blob(&repo, id, path)?;
        let total = data.iter().filter(|b| **b == b'\n').count() + usize::from(!data.is_empty() && !data.ends_with(b"\n"));
        let _ = &repo;
        let lines = git::blame(&ctx.dir, &id.to_string(), path, 2000)?;
        Ok(ctx.etag(&headers, format!("bl-{id}-{path}"), || {
            let body = html! {
                (ctx.header(ui::Tab::Overview, rev))
                div.stack {
                    (ui::crumbs(&ctx.base(), rev, path))
                    (ui::blame(&ctx.base(), rev, path, &lines, total))
                }
            };
            ctx.page(&format!("blame {path}"), body)
        }))
    })
    .await
}

async fn graph(State(app): St, Viewer(v): Viewer, Path((owner, name, rev)): Path<(String, String, String)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let id = git::resolve_commit(&ctx.open()?, &rev)?;
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&ctx.dir)
            .args(["log", "--graph", "--oneline", "--decorate", "-n", "200", &id.to_string()])
            .env_remove("GIT_DIR")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .map_err(|e| Err::Internal(e.to_string()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let body = html! {
            (ctx.header(ui::Tab::Commits, &rev))
            div.card.flush { pre.graph { (text) } }
        };
        Ok(ctx.page("graph", body))
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

async fn avatar_route(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let tag = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        format!("\"av-{:x}\"", h.finish())
    };
    let hit = headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes());
    let mut res = if hit {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "image/svg+xml")], ui::avatar(&name, 64).into_string()).into_response()
    };
    if let Ok(v) = HeaderValue::from_str(&tag) {
        res.headers_mut().insert(header::ETAG, v);
    }
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    res
}

async fn css(headers: HeaderMap) -> Response {
    let tag = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        CSS.hash(&mut h);
        format!("\"css-{:x}\"", h.finish())
    };
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

/// Client address for throttling. X-Forwarded-For is only believed when
/// LOOM_TRUST_PROXY is set (i.e. loom sits behind a reverse proxy).
pub fn client_ip(peer: Option<std::net::SocketAddr>, headers: &HeaderMap) -> String {
    if std::env::var("LOOM_TRUST_PROXY").is_ok_and(|v| v == "1") {
        if let Some(ip) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()) {
            return ip.trim().to_owned();
        }
    }
    peer.map_or_else(|| "unknown".into(), |p| p.ip().to_string())
}

async fn login(State(app): St, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, headers: HeaderMap, Form(f): Form<LoginF>) -> Res {
    let next = safe_next(f.next.as_deref());
    let a = app.clone();
    let name = f.name.trim().to_owned();
    let ip = client_ip(Some(peer), &headers);
    let result = blocking(move || Ok(a.login(&name, &f.password, &ip))).await?;
    let err = match result {
        Login::Ok(user) => {
            let raw = app.db.create_session(user.id)?;
            let mut res = Redirect::to(&next).into_response();
            res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, &raw, db::SESSION_DAYS * 86400));
            return Ok(res);
        }
        Login::NeedsTotp(user) => {
            let raw = app.db.create_pending_session(user.id)?;
            let mut res = Redirect::to("/login/totp").into_response();
            res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, &raw, 300));
            return Ok(res);
        }
        Login::Bad => "Wrong user name or password.",
        Login::Throttled => "Too many failed attempts. Try again in 15 minutes.",
    };
    Ok((StatusCode::UNAUTHORIZED, ui::page("sign in", None, ui::login(&next, Some(err)))).into_response())
}

async fn totp_page(State(app): St, headers: HeaderMap) -> Res {
    let pending = cookie(&headers, COOKIE).and_then(|raw| app.db.pending_session_user(raw).ok().flatten());
    match pending {
        Some(u) => Ok(ui::page("two-factor", None, ui::totp_login(&u.name, None)).into_response()),
        None => Ok(Redirect::to("/login").into_response()),
    }
}

#[derive(Deserialize)]
struct TotpF {
    code: String,
}

async fn totp_submit(State(app): St, headers: HeaderMap, Form(f): Form<TotpF>) -> Res {
    let Some(raw) = cookie(&headers, COOKIE).map(str::to_owned) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let Some(u) = app.db.pending_session_user(&raw)? else {
        return Ok(Redirect::to("/login").into_response());
    };
    let (secret, enabled) = app.db.totp(u.id)?;
    if enabled && totp::verify(&secret, &f.code, db::now()) {
        app.db.confirm_session(&raw)?;
        let mut res = Redirect::to("/").into_response();
        res.headers_mut().insert(header::SET_COOKIE, session_cookie(&app, &raw, db::SESSION_DAYS * 86400));
        return Ok(res);
    }
    Ok((StatusCode::UNAUTHORIZED, ui::page("two-factor", None, ui::totp_login(&u.name, Some("Wrong code.")))).into_response())
}

// ---------- two-factor settings ----------

async fn totp_settings(State(app): St, Authed(u): Authed) -> Res {
    let (secret, enabled) = app.db.totp(u.id)?;
    let body = if enabled {
        ui::totp_settings(&u.name, None, true, None)
    } else {
        // A fresh secret is generated for the form; enabling requires a code
        // from it, so an abandoned form changes nothing.
        let s = if secret.is_empty() { totp::new_secret() } else { secret };
        ui::totp_settings(&u.name, Some(&s), false, None)
    };
    Ok(ui::page("two-factor", Some(&u), body).into_response())
}

#[derive(Deserialize)]
struct TotpSetupF {
    secret: String,
    code: String,
}

async fn totp_enable(State(app): St, Authed(u): Authed, Form(f): Form<TotpSetupF>) -> Res {
    let (current, enabled) = app.db.totp(u.id)?;
    if enabled {
        return Ok(Redirect::to("/settings/2fa").into_response());
    }
    let secret = if current.is_empty() { f.secret.trim().to_uppercase() } else { current };
    if totp::base32_decode(&secret).is_none_or(|v| v.len() < 10) {
        return Ok((StatusCode::BAD_REQUEST, ui::page("two-factor", Some(&u), ui::totp_settings(&u.name, Some(&secret), false, Some("That secret doesn't look right; start over.")))).into_response());
    }
    if !totp::verify(&secret, &f.code, db::now()) {
        return Ok((StatusCode::BAD_REQUEST, ui::page("two-factor", Some(&u), ui::totp_settings(&u.name, Some(&secret), false, Some("Wrong code. Check your app's clock and try again.")))).into_response());
    }
    app.db.set_totp(u.id, &secret, true)?;
    Ok(ui::page("two-factor", Some(&u), ui::totp_settings(&u.name, None, true, Some("Two-factor authentication is on."))).into_response())
}

async fn totp_disable(State(app): St, Authed(u): Authed, Form(f): Form<TotpF>) -> Res {
    let (secret, enabled) = app.db.totp(u.id)?;
    if !enabled {
        return Ok(Redirect::to("/settings/2fa").into_response());
    }
    if !totp::verify(&secret, &f.code, db::now()) {
        return Ok((StatusCode::BAD_REQUEST, ui::page("two-factor", Some(&u), ui::totp_settings(&u.name, None, true, Some("Wrong code; not disabled.")))).into_response());
    }
    app.db.set_totp(u.id, "", false)?;
    Ok(ui::page("two-factor", Some(&u), ui::totp_settings(&u.name, None, false, Some("Two-factor authentication is off."))).into_response())
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

pub(crate) fn see_other(to: &str) -> Response {
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

async fn password_change(State(app): St, Authed(u): Authed, ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>, headers: HeaderMap, Form(f): Form<PasswordF>) -> Res {
    let fail = |msg: &str| Ok((StatusCode::BAD_REQUEST, ui::page("password", Some(&u), ui::password(None, Some(msg)))).into_response());
    if f.new.chars().count() < 8 {
        return fail("The new password must be at least 8 characters.");
    }
    let (a, name) = (app.clone(), u.name.clone());
    let ip = client_ip(Some(peer), &headers);
    let current = f.current;
    match blocking(move || Ok(a.login(&name, &current, &ip))).await? {
        Login::Ok(_) => {}
        Login::NeedsTotp(_) => {} // right password; 2FA is a separate step
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

async fn account_page(State(app): St, Authed(u): Authed) -> Res {
    let (email, on) = app.db.notify_prefs(u.id)?;
    Ok(ui::page("account", Some(&u), ui::account(&email, on, None, None)).into_response())
}

#[derive(Deserialize)]
struct AccountF {
    email: String,
    notify: Option<String>,
}

async fn account_save(State(app): St, Authed(u): Authed, Form(f): Form<AccountF>) -> Res {
    let email = f.email.trim();
    let enabled = f.notify.is_some() && !email.is_empty();
    let bad = (!email.is_empty() && (email.len() > 200 || !email.contains('@') || email.contains(char::is_whitespace)))
        .then_some("That doesn't look like an email address.");
    if let Some(e) = bad {
        return Ok((StatusCode::BAD_REQUEST, ui::page("account", Some(&u), ui::account(email, enabled, None, Some(e)))).into_response());
    }
    app.db.set_notify_email(u.id, email, enabled)?;
    let mut me = u.clone();
    me.email = email.to_owned();
    Ok(ui::page("account", Some(&me), ui::account(email, enabled, Some("Saved."), None)).into_response())
}

// ---------- new repo ----------

async fn new_page(State(app): St, Authed(u): Authed) -> Res {
    let owners = app.db.creatable_owners(&u)?;
    Ok(ui::page("new repository", Some(&u), ui::new_repo(&owners, &u.name, None)).into_response())
}

#[derive(Deserialize)]
struct NewF {
    owner: Option<String>,
    name: String,
    description: String,
    private: Option<String>,
}

async fn new_create(State(app): St, Authed(u): Authed, Form(f): Form<NewF>) -> Res {
    let owner = f.owner.clone().unwrap_or_else(|| u.name.clone());
    let (a, user, o) = (app.clone(), u.clone(), owner.clone());
    let res = blocking(move || Ok(a.create_repo(Some(&user), &o, f.name.trim(), &f.description, f.private.is_some()))).await?;
    match res {
        Ok(r) => Ok(see_other(&format!("/{}/{}", r.owner, r.name))),
        Err(e) => {
            let owners = app.db.creatable_owners(&u)?;
            Ok((StatusCode::BAD_REQUEST, ui::page("new repository", Some(&u), ui::new_repo(&owners, &owner, Some(&e)))).into_response())
        }
    }
}

// ---------- stars & forks ----------

async fn star(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    app.db.toggle_star(u.id, ctx.repo.id)?;
    Ok(see_other(&ctx.base()))
}

async fn fork_page(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let owners = app.db.creatable_owners(&u)?;
    Ok(ui::page("fork", Some(&u), ui::fork(&ctx.repo, &owners, None)).into_response())
}

#[derive(Deserialize)]
struct ForkF {
    owner: String,
    name: String,
}

async fn fork_create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<ForkF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let (a, user, src) = (app.clone(), u.clone(), ctx.repo.clone());
    let res = blocking(move || Ok(a.fork_repo(&user, &src, &f.owner, f.name.trim()))).await?;
    match res {
        Ok(r) => Ok(see_other(&format!("/{}/{}", r.owner, r.name))),
        Err(e) => {
            let owners = app.db.creatable_owners(&u)?;
            Ok((StatusCode::BAD_REQUEST, ui::page("fork", Some(&u), ui::fork(&ctx.repo, &owners, Some(&e)))).into_response())
        }
    }
}

// ---------- repo settings (admin) ----------

pub(crate) fn settings_page(app: &App, ctx: &Ctx, msg: Option<&str>, err: Option<&str>) -> Res {
    settings_response(app, ctx, msg, err)
}

fn settings_response(app: &App, ctx: &Ctx, msg: Option<&str>, err: Option<&str>) -> Res {
    let u = ctx.viewer.as_ref().ok_or(Err::NotFound)?;
    let branches = git::branch_names(&ctx.dir);
    let default = git::open(&ctx.dir).ok().and_then(|r| git::default_branch(&r));
    let collaborators = app.db.collaborators(ctx.repo.id)?;
    let protections = app.db.protections(ctx.repo.id)?;
    let mut owners = app.db.creatable_owners(u)?;
    if !owners.iter().any(|o| o.eq_ignore_ascii_case(&ctx.repo.owner)) {
        owners.insert(0, ctx.repo.owner.clone());
    }
    let teams = app.db.repo_teams(ctx.repo.id)?;
    let data = ui::SettingsData {
        teams: &teams,
        branches: &branches,
        default_branch: default.as_deref(),
        collaborators: &collaborators,
        protections: &protections,
        owners: &owners,
    };
    let rev = default.clone().unwrap_or_else(|| "HEAD".into());
    let hooks: Vec<_> = app
        .db
        .webhooks(ctx.repo.id)?
        .into_iter()
        .map(|h| {
            let last = app.db.last_delivery_status(h.id).ok().flatten();
            (h, last)
        })
        .collect();
    let sbase = format!("{}/settings", ctx.base());
    let mirror = crate::mirrors::section(&ctx.repo, &app.db, &sbase);
    let body = html! {
        (ctx.header(ui::Tab::Settings, &rev))
        (ui::repo_settings(&ctx.repo, &data, msg, err))
        div.mt { (crate::ui_social::hooks_section(&sbase, &hooks)) }
        div.mt { (mirror) }
    };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page("settings", body)).into_response())
}

async fn admin_ctx(app: &Arc<App>, u: User, owner: &str, name: &str) -> Result<Ctx, Err> {
    Ctx::load_with(app, Some(u), owner, name, Access::Admin)
}

async fn repo_settings(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    settings_response(&app, &ctx, None, None)
}

#[derive(Deserialize)]
struct GeneralF {
    description: String,
    default_branch: Option<String>,
    private: Option<String>,
    archived: Option<String>,
    show_last_commit: Option<String>,
}

async fn settings_general(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<GeneralF>) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    app.db.update_repo(ctx.repo.id, f.description.trim(), f.private.is_some(), f.archived.is_some(), f.show_last_commit.is_some())?;
    if let Some(b) = f.default_branch.filter(|b| !b.is_empty()) {
        if let Err(e) = git::set_default_branch(&ctx.dir, &b) {
            let ctx = admin_ctx(&app, u, &owner, &name).await?;
            return settings_response(&app, &ctx, None, Some(&e));
        }
    }
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    settings_response(&app, &ctx, Some("Saved."), None)
}

#[derive(Deserialize)]
struct CollabF {
    name: String,
    role: Option<String>,
}

async fn settings_collab_add(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<CollabF>) -> Res {
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    let role = crate::cli::parse_role(f.role.as_deref().unwrap_or("write"));
    let target = app.db.user_by_name(f.name.trim())?;
    match (target, role) {
        (Some(t), Some(r)) if !t.name.eq_ignore_ascii_case(&ctx.repo.owner) => {
            app.db.set_collaborator(ctx.repo.id, t.id, r)?;
            settings_response(&app, &ctx, Some(&format!("{} now has {} access.", t.name, f.role.as_deref().unwrap_or("write"))), None)
        }
        (Some(_), Some(_)) => settings_response(&app, &ctx, None, Some("The owner already has full access.")),
        (None, _) => settings_response(&app, &ctx, None, Some(&format!("No user named '{}'.", f.name.trim()))),
        (_, None) => settings_response(&app, &ctx, None, Some("Role must be read, write or admin.")),
    }
}

async fn settings_collab_remove(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<CollabF>) -> Res {
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    app.db.remove_collaborator(ctx.repo.id, &f.name)?;
    settings_response(&app, &ctx, Some(&format!("Removed {}.", f.name)), None)
}

#[derive(Deserialize)]
struct ProtectF {
    branch: String,
    allow_force_push: Option<String>,
    allow_delete: Option<String>,
    require_pr: Option<String>,
    required_checks: Option<String>,
}

async fn settings_protect(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<ProtectF>) -> Res {
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    let branch = f.branch.trim().trim_start_matches("refs/heads/");
    let valid = !branch.is_empty() && branch.len() <= 200 && !branch.contains("..") && !branch.starts_with('-')
        && branch.bytes().all(|b| b > b' ' && !b"~^:?*[\\".contains(&b));
    if !valid {
        return settings_response(&app, &ctx, None, Some("Not a valid branch name."));
    }
    let p = crate::db::Protection {
        branch: branch.to_owned(),
        allow_force_push: f.allow_force_push.is_some(),
        allow_delete: f.allow_delete.is_some(),
        require_pr: f.require_pr.is_some(),
        required_checks: f
            .required_checks
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty() && c.len() <= 100)
            .take(20)
            .map(str::to_owned)
            .collect(),
    };
    app.db.set_protection(ctx.repo.id, &p)?;
    settings_response(&app, &ctx, Some(&format!("'{branch}' is protected.")), None)
}

async fn settings_unprotect(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<ProtectF>) -> Res {
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    app.db.remove_protection(ctx.repo.id, &f.branch)?;
    settings_response(&app, &ctx, Some(&format!("'{}' is no longer protected.", f.branch)), None)
}

#[derive(Deserialize)]
struct MoveF {
    owner: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct MirrorF {
    url: String,
    hours: Option<i64>,
}

async fn mirror_set(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<MirrorF>) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    let url = f.url.trim().to_owned();
    if !crate::gitops::valid_mirror_url(&url) {
        return settings_response(&app, &ctx, None, Some("Mirror URLs must start with http://, https://, ssh:// or git://."));
    }
    let hours = f.hours.unwrap_or(6).clamp(1, 168);
    app.db.set_mirror_config(ctx.repo.id, &url, hours)?;
    app.db.set_mirror(ctx.repo.id, true)?;
    // First sync right away, synchronously, so the admin sees errors here.
    let (a, id) = (app.clone(), ctx.repo.id);
    let r = blocking(move || {
        crate::mirrors::sync(&a, id, &url);
        Ok(())
    })
    .await;
    let ctx = admin_ctx(&app, u, &owner, &name).await?;
    match r.map_err(|e| match e {
        Err::Internal(m) => m,
        _ => "sync failed".into(),
    }) {
        Ok(()) => settings_response(&app, &ctx, Some("Mirror configured; the first sync ran. Check the status below."), None),
        Err(e) => settings_response(&app, &ctx, None, Some(&format!("Mirror configured but the first sync failed: {e}"))),
    }
}

async fn mirror_delete(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    app.db.remove_mirror(ctx.repo.id)?;
    app.db.set_mirror(ctx.repo.id, false)?;
    settings_response(&app, &ctx, Some("Mirror removed; the repo accepts pushes again."), None)
}

async fn mirror_sync(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    let Some((url, _, _, _, _)) = app.db.mirror(ctx.repo.id)? else {
        return settings_response(&app, &ctx, None, Some("No mirror configured."));
    };
    let (a, id) = (app.clone(), ctx.repo.id);
    blocking(move || {
        crate::mirrors::sync(&a, id, &url);
        Ok(())
    })
    .await?;
    settings_response(&app, &ctx, Some("Sync finished."), None)
}

async fn settings_move(app: Arc<App>, u: User, owner: String, name: String, f: MoveF) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    let to_owner = f.owner.unwrap_or_else(|| ctx.repo.owner.clone());
    let to_name = f.name.map(|n| n.trim().to_owned()).unwrap_or_else(|| ctx.repo.name.clone());
    let (a, user, repo) = (app.clone(), u, ctx.repo.clone());
    match blocking(move || Ok(a.move_repo(&user, &repo, &to_owner, &to_name))).await? {
        Ok(r) => Ok(see_other(&format!("/{}/{}/settings", r.owner, r.name))),
        Err(e) => settings_response(&app, &ctx, None, Some(&e)),
    }
}

async fn settings_rename(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<MoveF>) -> Res {
    settings_move(app, u, owner, name, MoveF { owner: None, name: f.name }).await
}

async fn settings_transfer(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<MoveF>) -> Res {
    settings_move(app, u, owner, name, MoveF { owner: f.owner, name: None }).await
}

#[derive(Deserialize)]
struct ConfirmF {
    confirm: String,
}

async fn settings_delete(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<ConfirmF>) -> Res {
    let ctx = admin_ctx(&app, u.clone(), &owner, &name).await?;
    let full = format!("{}/{}", ctx.repo.owner, ctx.repo.name);
    if f.confirm.trim() != full {
        return settings_response(&app, &ctx, None, Some(&format!("Type {full} exactly to confirm.")));
    }
    let (a, repo) = (app.clone(), ctx.repo.clone());
    match blocking(move || Ok(a.delete_repo(&repo))).await? {
        Ok(()) => Ok(see_other(&format!("/{}", ctx.repo.owner))),
        Err(e) => settings_response(&app, &ctx, None, Some(&e)),
    }
}

// ---------- orgs ----------

async fn new_org_page(Authed(u): Authed) -> Response {
    ui::page("new organization", Some(&u), ui::new_org(None)).into_response()
}

#[derive(Deserialize)]
struct OrgF {
    name: String,
    description: String,
}

async fn new_org(State(app): St, Authed(u): Authed, Form(f): Form<OrgF>) -> Res {
    let name = f.name.trim();
    let err = if !db::valid_name(name) {
        "Names may use letters, digits, '.', '_' and '-'.".to_owned()
    } else if app.db.owner(name)?.is_some() {
        format!("'{name}' is already taken.")
    } else {
        app.db.create_org(name, f.description.trim(), u.id)?;
        return Ok(see_other(&format!("/{name}")));
    };
    Ok((StatusCode::BAD_REQUEST, ui::page("new organization", Some(&u), ui::new_org(Some(&err)))).into_response())
}

/// The org, if `u` administers it (or is a site admin); otherwise 404.
fn admin_org(app: &App, u: &User, name: &str) -> Result<crate::db::Owner, Err> {
    let o = app.db.owner(name)?.filter(|o| o.is_org).ok_or(Err::NotFound)?;
    if u.is_admin || app.db.org_role(&o.name, u.id)? == Access::Admin {
        Ok(o)
    } else {
        Err(Err::NotFound)
    }
}

fn org_settings_response(app: &App, u: &User, org: &str, msg: Option<&str>, err: Option<&str>) -> Res {
    let o = admin_org(app, u, org)?;
    let members = app.db.members(o.id)?;
    let n = app.db.repo_count(&o.name)?;
    let teams: Vec<ui::TeamRow> = app
        .db
        .teams(o.id)?
        .into_iter()
        .map(|(id, name, description)| {
            let members = app.db.team_members(id).map(|m| m.len()).unwrap_or(0);
            let repos = app.db.team_repos(id).map(|r| r.len()).unwrap_or(0);
            ui::TeamRow { id, name, description, members, repos }
        })
        .collect();
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ui::page("org settings", Some(u), ui::org_settings(&o, &members, n, &teams, msg, err))).into_response())
}

async fn org_settings(State(app): St, Authed(u): Authed, Path(org): Path<String>) -> Res {
    org_settings_response(&app, &u, &org, None, None)
}

#[derive(Deserialize)]
struct OrgGeneralF {
    description: String,
}

async fn org_general(State(app): St, Authed(u): Authed, Path(org): Path<String>, Form(f): Form<OrgGeneralF>) -> Res {
    let o = admin_org(&app, &u, &org)?;
    app.db.update_org(o.id, f.description.trim())?;
    org_settings_response(&app, &u, &org, Some("Saved."), None)
}

async fn org_member_add(State(app): St, Authed(u): Authed, Path(org): Path<String>, Form(f): Form<CollabF>) -> Res {
    let o = admin_org(&app, &u, &org)?;
    let Some(role) = crate::cli::parse_role(f.role.as_deref().unwrap_or("write")) else {
        return org_settings_response(&app, &u, &org, None, Some("Role must be read, write or admin."));
    };
    let Some(t) = app.db.user_by_name(f.name.trim())? else {
        return org_settings_response(&app, &u, &org, None, Some(&format!("No user named '{}'.", f.name.trim())));
    };
    let current = app.db.org_role(&o.name, t.id)?;
    if current == Access::Admin && role != Access::Admin && app.db.admin_count(o.id)? <= 1 {
        return org_settings_response(&app, &u, &org, None, Some("An org needs at least one admin."));
    }
    app.db.set_member(o.id, t.id, role)?;
    org_settings_response(&app, &u, &org, Some(&format!("{} updated.", t.name)), None)
}

async fn org_member_remove(State(app): St, Authed(u): Authed, Path(org): Path<String>, Form(f): Form<CollabF>) -> Res {
    let o = admin_org(&app, &u, &org)?;
    let is_admin = app.db.user_by_name(&f.name)?.map(|t| app.db.org_role(&o.name, t.id)).transpose()? == Some(Access::Admin);
    if is_admin && app.db.admin_count(o.id)? <= 1 {
        return org_settings_response(&app, &u, &org, None, Some("An org needs at least one admin."));
    }
    app.db.remove_member(o.id, &f.name)?;
    org_settings_response(&app, &u, &org, Some(&format!("Removed {}.", f.name)), None)
}

#[derive(Deserialize)]
struct TeamF {
    name: String,
    description: Option<String>,
}

async fn team_create(State(app): St, Authed(u): Authed, Path(org): Path<String>, Form(f): Form<TeamF>) -> Res {
    let o = admin_org(&app, &u, &org)?;
    let name = f.name.trim();
    let bad = (name.is_empty() || name.len() > 50 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b)))
        .then_some("Team names: letters, digits, spaces, . _ - (max 50).");
    if let Some(e) = bad {
        return org_settings_response(&app, &u, &org, None, Some(e));
    }
    match app.db.create_team(o.id, name, f.description.as_deref().unwrap_or("").trim()) {
        Ok(_) => org_settings_response(&app, &u, &org, Some("Team created."), None),
        Err(e) if crate::issues_web::is_unique_violation(&e) => org_settings_response(&app, &u, &org, None, Some("A team with that name exists.")),
        Err(e) => Err(e.into()),
    }
}

/// The team, if it belongs to an org this user administers.
fn team_ctx(app: &App, u: &User, org: &str, id: i64) -> Result<(crate::db::Owner, ui::TeamRow), Err> {
    let o = admin_org(app, u, org)?;
    let (_, name, description) = app.db.teams(o.id)?.into_iter().find(|(tid, _, _)| *tid == id).ok_or(Err::NotFound)?;
    let t = ui::TeamRow { id, name, description, members: 0, repos: 0 };
    Ok((o, t))
}

fn team_response(app: &App, u: &User, org: &str, id: i64, msg: Option<&str>, err: Option<&str>) -> Res {
    let (o, mut team) = team_ctx(app, u, org, id)?;
    let members = app.db.team_members(id)?;
    let repos = app.db.team_repos(id)?;
    team.members = members.len();
    team.repos = repos.len();
    let all: Vec<String> = app.db.visible_repos(Some(u), Some(&o.name))?.into_iter().map(|r| format!("{}/{}", r.owner, r.name)).collect();
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ui::page("team", Some(u), ui::team_page(&o, &team, &members, &repos, &all, msg, err))).into_response())
}

async fn team_page(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>) -> Res {
    team_response(&app, &u, &org, id, None, None)
}

async fn team_member_add(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>, Form(f): Form<CollabF>) -> Res {
    team_ctx(&app, &u, &org, id)?;
    let Some(role) = crate::cli::parse_role(f.role.as_deref().unwrap_or("write")) else {
        return team_response(&app, &u, &org, id, None, Some("Role must be read, write or admin."));
    };
    let Some(t) = app.db.user_by_name(f.name.trim())? else {
        return team_response(&app, &u, &org, id, None, Some(&format!("No user named '{}'.", f.name.trim())));
    };
    app.db.set_team_member(id, t.id, role)?;
    team_response(&app, &u, &org, id, Some(&format!("{} updated.", t.name)), None)
}

async fn team_member_remove(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>, Form(f): Form<CollabF>) -> Res {
    team_ctx(&app, &u, &org, id)?;
    app.db.remove_team_member(id, &f.name)?;
    team_response(&app, &u, &org, id, Some(&format!("Removed {}.", f.name)), None)
}

#[derive(Deserialize)]
struct TeamRepoF {
    repo: String,
    role: Option<String>,
}

async fn team_repo_grant(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>, Form(f): Form<TeamRepoF>) -> Res {
    let (o, _) = team_ctx(&app, &u, &org, id)?;
    let Some(role) = crate::cli::parse_role(f.role.as_deref().unwrap_or("write")) else {
        return team_response(&app, &u, &org, id, None, Some("Role must be read, write or admin."));
    };
    // The form sends "owner/name"; only the org's own repositories are valid.
    let Some((ro, rn)) = f.repo.trim().split_once('/') else {
        return team_response(&app, &u, &org, id, None, Some("Pick one of the org's repositories."));
    };
    let Some(repo) = app.db.repo(ro, rn)? else {
        return team_response(&app, &u, &org, id, None, Some("Pick one of the org's repositories."));
    };
    if !ro.eq_ignore_ascii_case(&o.name) {
        return team_response(&app, &u, &org, id, None, Some("Pick one of the org's repositories."));
    }
    app.db.grant_team_repo(id, repo.id, role)?;
    team_response(&app, &u, &org, id, Some(&format!("{} granted.", f.repo.trim())), None)
}

async fn team_repo_revoke(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>, Form(f): Form<TeamRepoF>) -> Res {
    team_ctx(&app, &u, &org, id)?;
    app.db.revoke_team_repo(id, f.repo.trim())?;
    team_response(&app, &u, &org, id, Some("Revoked."), None)
}

async fn team_delete(State(app): St, Authed(u): Authed, Path((org, id)): Path<(String, i64)>, Form(f): Form<ConfirmF>) -> Res {
    let (o, team) = team_ctx(&app, &u, &org, id)?;
    if f.confirm.trim() != team.name {
        return team_response(&app, &u, &org, id, None, Some(&format!("Type {} exactly to confirm.", team.name)));
    }
    app.db.delete_team(o.id, id)?;
    org_settings_response(&app, &u, &org, Some("Team deleted."), None)
}

async fn org_delete(State(app): St, Authed(u): Authed, Path(org): Path<String>, Form(f): Form<ConfirmF>) -> Res {
    let o = admin_org(&app, &u, &org)?;
    if app.db.repo_count(&o.name)? > 0 {
        return org_settings_response(&app, &u, &org, None, Some("Move or delete its repositories first."));
    }
    if f.confirm.trim() != o.name {
        return org_settings_response(&app, &u, &org, None, Some(&format!("Type {} exactly to confirm.", o.name)));
    }
    app.db.delete_org(o.id)?;
    Ok(see_other("/"))
}

async fn fallback() -> Response {
    (StatusCode::NOT_FOUND, ui::not_found()).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(css))
        .route("/avatar/{name}", get(avatar_route))
        .route("/login", get(login_page).post(login))
        .route("/login/totp", get(totp_page).post(totp_submit))
        .route("/logout", post(logout))
        .route("/new", get(new_page).post(new_create))
        .route("/new/org", get(new_org_page).post(new_org))
        .route("/settings", get(|| async { Redirect::to("/settings/keys") }))
        .route("/settings/keys", get(keys_page).post(keys_add))
        .route("/settings/keys/{id}/delete", post(keys_delete))
        .route("/settings/tokens", get(tokens_page).post(tokens_add))
        .route("/settings/tokens/{id}/delete", post(tokens_delete))
        .route("/settings/password", get(password_page).post(password_change))
        .route("/settings/account", get(account_page).post(account_save))
        .route("/settings/2fa", get(totp_settings))
        .route("/settings/2fa/enable", post(totp_enable))
        .route("/settings/2fa/disable", post(totp_disable))
        .route("/{owner}", get(owner))
        .route("/{owner}/settings", get(org_settings))
        .route("/{owner}/settings/general", post(org_general))
        .route("/{owner}/settings/members", post(org_member_add))
        .route("/{owner}/settings/members/remove", post(org_member_remove))
        .route("/{owner}/settings/teams", post(team_create))
        .route("/{owner}/settings/teams/{id}", get(team_page))
        .route("/{owner}/settings/teams/{id}/members", post(team_member_add))
        .route("/{owner}/settings/teams/{id}/members/remove", post(team_member_remove))
        .route("/{owner}/settings/teams/{id}/repos", post(team_repo_grant))
        .route("/{owner}/settings/teams/{id}/repos/remove", post(team_repo_revoke))
        .route("/{owner}/settings/teams/{id}/delete", post(team_delete))
        .route("/{owner}/settings/delete", post(org_delete))
        .route("/{owner}/{name}", get(overview))
        .route("/{owner}/{name}/info/refs", get(githttp::info_refs))
        .route("/{owner}/{name}/git-upload-pack", post(githttp::upload_pack))
        .route("/{owner}/{name}/git-receive-pack", post(githttp::receive_pack))
        .route("/{owner}/{name}/tree/{*rest}", get(tree))
        .route("/{owner}/{name}/blob/{*rest}", get(blob))
        .route("/{owner}/{name}/raw/{*rest}", get(raw))
        .route("/{owner}/{name}/blame/{*rest}", get(blame))
        .route("/{owner}/{name}/log/{*rev}", get(log))
        .route("/{owner}/{name}/commits", get(commits_alias))
        .route("/{owner}/{name}/commits/{*rev}", get(commits_alias_rev))
        .route("/{owner}/{name}/graph/{*rev}", get(graph))
        .route("/{owner}/{name}/commit/{rev}", get(commit))
        .route("/{owner}/{name}/refs", get(refs))
        .route("/{owner}/{name}/star", post(star))
        .route("/{owner}/{name}/fork", get(fork_page).post(fork_create))
        .route("/{owner}/{name}/settings", get(repo_settings))
        .route("/{owner}/{name}/settings/general", post(settings_general))
        .route("/{owner}/{name}/settings/collaborators", post(settings_collab_add))
        .route("/{owner}/{name}/settings/collaborators/remove", post(settings_collab_remove))
        .route("/{owner}/{name}/settings/protect", post(settings_protect))
        .route("/{owner}/{name}/settings/protect/remove", post(settings_unprotect))
        .route("/{owner}/{name}/settings/rename", post(settings_rename))
        .route("/{owner}/{name}/settings/transfer", post(settings_transfer))
        .route("/{owner}/{name}/settings/delete", post(settings_delete))
        .route("/{owner}/{name}/settings/mirror", post(mirror_set))
        .route("/{owner}/{name}/settings/mirror/sync", post(mirror_sync))
        .route("/{owner}/{name}/settings/mirror/delete", post(mirror_delete))
        .merge(crate::issues_web::routes())
        .merge(crate::pulls_web::routes())
        .merge(crate::api::routes())
        .merge(crate::feeds::routes())
        .merge(crate::social_web::routes())
        .merge(crate::releases::routes())
        .merge(crate::lfs::routes())
        .merge(crate::ops::routes())
        .fallback(fallback)
        .layer(middleware::from_fn(same_origin))
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn_with_state(app.clone(), crate::ops::count))
        .layer(middleware::from_fn_with_state(app.clone(), crate::ops::rate_limit))
        .layer(middleware::from_fn(crate::ops::compress))
        .with_state(app)
}
