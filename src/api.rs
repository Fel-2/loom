//! JSON API (v1). Auth: `Authorization: Bearer <token>` or HTTP Basic with a
//! token as the password. Cookies are ignored, so browsers can't be tricked
//! into calling it (no CSRF surface). Reads of public repos are anonymous.

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::App;
use crate::db::{Access, Repo, User};
use crate::social_db::{Status, STATES};

type St = State<Arc<App>>;
type ApiRes = Result<Response, ApiErr>;

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "message": self.1 }))).into_response()
    }
}

impl From<crate::db::Error> for ApiErr {
    fn from(e: crate::db::Error) -> Self {
        eprintln!("api: {e}");
        ApiErr(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

fn not_found() -> ApiErr {
    ApiErr(StatusCode::NOT_FOUND, "Not Found".into())
}

impl From<crate::web::Err> for ApiErr {
    fn from(e: crate::web::Err) -> Self {
        match e {
            crate::web::Err::NotFound => not_found(),
            crate::web::Err::Internal(m) => {
                eprintln!("api: {m}");
                ApiErr(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

/// Like `web::blocking` but for `Result<_, ApiErr>` (git work is blocking).
async fn block<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiErr> + Send + 'static) -> Result<T, ApiErr> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiErr(StatusCode::INTERNAL_SERVER_ERROR, format!("internal error: {e}")))?
}

fn bad(msg: &str) -> ApiErr {
    ApiErr(StatusCode::UNPROCESSABLE_ENTITY, msg.into())
}

/// Token user, if any. A present-but-invalid token is an error, not anonymous.
pub struct ApiUser(pub Option<User>);

impl FromRequestParts<Arc<App>> for ApiUser {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        let Some(v) = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return Ok(ApiUser(None));
        };
        let token = if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("token ")) {
            t.trim().to_owned()
        } else if let Some(b) = v.strip_prefix("Basic ") {
            let raw = base64::engine::general_purpose::STANDARD.decode(b.trim()).unwrap_or_default();
            String::from_utf8_lossy(&raw).split_once(':').map(|(_, p)| p.to_owned()).unwrap_or_default()
        } else {
            String::new()
        };
        match app.db.token_user(&token)? {
            Some(u) if token.starts_with("loom_") => Ok(ApiUser(Some(u))),
            _ => Err(ApiErr(StatusCode::UNAUTHORIZED, "Bad credentials".into())),
        }
    }
}

fn repo_for(app: &App, user: Option<&User>, owner: &str, name: &str, need: Access) -> Result<Repo, ApiErr> {
    match app.authorize(user, owner, name, need) {
        Some((r, _)) => Ok(r),
        // Authenticated but lacking write on a visible repo: say so.
        None if need > Access::Read && app.authorize(user, owner, name, Access::Read).is_some() => {
            Err(ApiErr(StatusCode::FORBIDDEN, "Write access required".into()))
        }
        None => Err(not_found()),
    }
}

fn repo_json(app: &App, r: &Repo) -> Value {
    let dir = app.repo_dir(r);
    let default = crate::git::open(&dir).ok().and_then(|g| crate::git::default_branch(&g));
    json!({
        "id": r.id, "name": r.name, "full_name": format!("{}/{}", r.owner, r.name),
        "owner": { "login": r.owner }, "description": r.description, "private": r.private,
        "archived": r.archived, "fork": r.fork_of.is_some(), "default_branch": default,
        "html_url": format!("{}/{}/{}", app.url, r.owner, r.name),
        "clone_url": format!("{}/{}/{}.git", app.url, r.owner, r.name),
    })
}

fn issue_json(app: &App, r: &Repo, i: &crate::issues_db::Issue) -> Value {
    let kind = if i.is_pr { "pulls" } else { "issues" };
    json!({
        "number": i.number, "title": i.title, "body": i.body,
        "state": if i.closed { "closed" } else { "open" },
        "user": { "login": i.author }, "comments": i.comments,
        "labels": i.labels.iter().map(|l| json!({ "name": l.name, "color": l.color })).collect::<Vec<_>>(),
        "milestone": i.milestone, "created_at": i.created, "updated_at": i.updated, "closed_at": i.closed_at,
        "pull_request": i.is_pr, "merged": i.merged, "draft": i.draft,
        "html_url": format!("{}/{}/{}/{kind}/{}", app.url, r.owner, r.name, i.number),
    })
}

async fn me(ApiUser(u): ApiUser) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    Ok(Json(json!({ "login": u.name, "id": u.id, "email": u.email, "site_admin": u.is_admin })).into_response())
}

async fn repo(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    Ok(Json(repo_json(&app, &r)).into_response())
}

async fn branches(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let dir = app.repo_dir(&r);
    let list: Vec<Value> = crate::git::branch_names(&dir)
        .into_iter()
        .map(|b| {
            let sha = crate::gitops::branch_sha(&dir, &b);
            json!({ "name": b, "commit": { "sha": sha } })
        })
        .collect();
    Ok(Json(list).into_response())
}

#[derive(Deserialize)]
struct ListQ {
    state: Option<String>,
    label: Option<String>,
    page: Option<usize>,
}

async fn list(app: &App, u: Option<&User>, o: &str, n: &str, q: ListQ, is_pr: bool) -> ApiRes {
    let r = repo_for(app, u, o, n, Access::Read)?;
    let f = crate::issues_db::Filter { closed: q.state.as_deref() == Some("closed"), label: q.label, ..Default::default() };
    let page = q.page.unwrap_or(1).max(1);
    let items = app.db.list_issues(r.id, is_pr, &f, 50, (page - 1) * 50)?;
    Ok(Json(items.iter().map(|i| issue_json(app, &r, i)).collect::<Vec<_>>()).into_response())
}

async fn issues(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Query(q): Query<ListQ>) -> ApiRes {
    list(&app, u.as_ref(), &o, &n, q, false).await
}

async fn pulls(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Query(q): Query<ListQ>) -> ApiRes {
    list(&app, u.as_ref(), &o, &n, q, true).await
}

async fn issue(State(app): St, ApiUser(u): ApiUser, Path((o, n, num)): Path<(String, String, i64)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    let mut v = issue_json(&app, &r, &i);
    if let Some(p) = app.db.pull(r.id, num)? {
        v["head"] = json!({ "ref": p.head_branch, "sha": p.head_sha });
        v["base"] = json!({ "ref": p.base_branch });
        v["merge_commit_sha"] = json!(p.merge_sha);
        v["draft"] = json!(p.draft);
    }
    Ok(Json(v).into_response())
}

#[derive(Deserialize)]
struct NewIssue {
    title: String,
    #[serde(default)]
    body: String,
}

async fn create_issue(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewIssue>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Read)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    if b.title.trim().is_empty() || b.title.chars().count() > 256 || b.body.len() > 65536 {
        return Err(bad("title must be 1-256 characters, body at most 64 KB"));
    }
    let (_, num) = app.db.create_issue(r.id, u.id, b.title.trim(), &b.body, false)?;
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    crate::issues_web::record_mentions(&app, &u, &r, &i, &b.body);
    crate::events::emit(&app.bus(), crate::events::Ev::Issue { repo: &r, actor: &u, issue: &i, action: "opened" });
    Ok((StatusCode::CREATED, Json(issue_json(&app, &r, &i))).into_response())
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

async fn create_comment(State(app): St, ApiUser(u): ApiUser, Path((o, n, num)): Path<(String, String, i64)>, Json(b): Json<NewComment>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Read)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    if b.body.trim().is_empty() || b.body.len() > 65536 {
        return Err(bad("body must be 1 byte - 64 KB"));
    }
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    let id = app.db.add_comment(i.id, u.id, &b.body)?;
    crate::issues_web::record_mentions(&app, &u, &r, &i, &b.body);
    crate::events::emit(&app.bus(), crate::events::Ev::Comment { repo: &r, actor: &u, issue: &i, body: &b.body });
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "body": b.body, "user": { "login": u.name } }))).into_response())
}

// ---------- issue editing ----------

#[derive(Deserialize)]
struct EditIssue {
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    state: Option<String>,
    labels: Option<Vec<String>>,
    milestone: Option<String>,
}

async fn edit_issue(
    State(app): St,
    ApiUser(u): ApiUser,
    Path((o, n, num)): Path<(String, String, i64)>,
    Json(b): Json<EditIssue>,
) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Read)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    let can_write = app.authorize(Some(&u), &o, &n, Access::Write).is_some();
    if !u.is_admin && Some(u.id) != i.author_id && !can_write {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Only the author or people with write access can edit this".into()));
    }
    let title = b.title.as_deref().unwrap_or(&i.title);
    if title.trim().is_empty() || title.chars().count() > 256 {
        return Err(bad("title must be 1-256 characters"));
    }
    let body = b.body.as_deref().unwrap_or(&i.body);
    if body.len() > 65536 {
        return Err(bad("body at most 64 KB"));
    }
    if b.title.is_some() || b.body.is_some() {
        if b.body.is_some() && body != i.body {
            crate::issues_web::record_mentions(&app, &u, &r, &i, body);
        }
        app.db.edit_issue(&i, u.id, title.trim(), body)?;
    }
    if let Some(names) = &b.labels {
        let all = app.db.labels(r.id)?;
        let mut ids = Vec::new();
        for want in names {
            let Some(l) = all.iter().find(|l| l.name.eq_ignore_ascii_case(want.trim())) else {
                return Err(bad(&format!("no label named '{want}'")));
            };
            ids.push(l.id);
        }
        app.db.set_issue_labels(r.id, i.id, &ids, u.id)?;
    }
    if let Some(m) = &b.milestone {
        let mid = if m.trim().is_empty() {
            None
        } else {
            match app.db.milestones(r.id)?.into_iter().find(|x| x.title.eq_ignore_ascii_case(m.trim())) {
                Some(x) => Some(x.id),
                None => return Err(bad(&format!("no milestone named '{m}'"))),
            }
        };
        app.db.set_issue_milestone(r.id, &i, mid, u.id)?;
    }
    if let Some(state) = &b.state {
        let closed = match state.as_str() {
            "open" => false,
            "closed" => true,
            _ => return Err(bad("state must be open or closed")),
        };
        if app.db.set_issue_closed(i.id, closed, Some(u.id), if closed { "closed" } else { "reopened" }, "", None)? {
            let fresh = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
            match app.db.pull(r.id, num)? {
                Some(p) => crate::events::emit(&app.bus(), crate::events::Ev::Pull {
                    repo: &r,
                    actor: Some(&u),
                    issue: &fresh,
                    pull: &p,
                    action: if closed { "closed" } else { "reopened" },
                }),
                None => crate::events::emit(&app.bus(), crate::events::Ev::Issue {
                    repo: &r,
                    actor: &u,
                    issue: &fresh,
                    action: if closed { "closed" } else { "reopened" },
                }),
            }
        }
    }
    let fresh = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    Ok(Json(issue_json(&app, &r, &fresh)).into_response())
}

async fn list_comments(State(app): St, ApiUser(u): ApiUser, Path((o, n, num)): Path<(String, String, i64)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    let list = app.db.comments(i.id)?;
    Ok(Json(
        list.iter()
            .map(|c| {
                json!({
                    "id": c.id, "body": c.body, "user": { "login": c.author },
                    "created_at": c.created, "updated_at": c.updated,
                })
            })
            .collect::<Vec<_>>(),
    )
    .into_response())
}

// ---------- labels & milestones ----------

fn label_json(l: &crate::issues_db::Label) -> Value {
    json!({ "id": l.id, "name": l.name, "color": l.color, "description": l.description })
}

async fn labels(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    Ok(Json(app.db.labels(r.id)?.iter().map(label_json).collect::<Vec<_>>()).into_response())
}

#[derive(Deserialize)]
struct NewLabel {
    name: String,
    color: String,
    #[serde(default)]
    description: String,
}

/// GitHub sends label colours without the leading '#'; the UI's color input
/// always includes it. Accept both, store the canonical form.
fn normalise_color(c: &str) -> String {
    let c = c.trim();
    if c.starts_with('#') { c.to_owned() } else { format!("#{c}") }
}

async fn create_label(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewLabel>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    let color = normalise_color(&b.color);
    if b.name.trim().is_empty() || b.name.chars().count() > 50 || !crate::issues_db::valid_color(&color) || b.description.len() > 200 {
        return Err(bad("name 1-50 chars, color '#rrggbb' (or 'rrggbb'), description at most 200 chars"));
    }
    match app.db.create_label(r.id, b.name.trim(), &color, b.description.trim()) {
        Ok(()) => {}
        Err(crate::db::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            return Err(ApiErr(StatusCode::CONFLICT, "A label with that name exists".into()));
        }
        Err(e) => return Err(e.into()),
    }
    let l = app.db.labels(r.id)?.into_iter().find(|l| l.name.eq_ignore_ascii_case(b.name.trim())).ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(label_json(&l))).into_response())
}

async fn edit_label(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>, Json(b): Json<NewLabel>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    let color = normalise_color(&b.color);
    if b.name.trim().is_empty() || b.name.chars().count() > 50 || !crate::issues_db::valid_color(&color) || b.description.len() > 200 {
        return Err(bad("name 1-50 chars, color '#rrggbb' (or 'rrggbb'), description at most 200 chars"));
    }
    if app.db.labels(r.id)?.iter().all(|l| l.id != id) {
        return Err(not_found());
    }
    app.db.update_label(r.id, id, b.name.trim(), &color, b.description.trim())?;
    let l = app.db.labels(r.id)?.into_iter().find(|l| l.id == id).ok_or_else(not_found)?;
    Ok(Json(label_json(&l)).into_response())
}

async fn delete_label(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if app.db.labels(r.id)?.iter().all(|l| l.id != id) {
        return Err(not_found());
    }
    app.db.delete_label(r.id, id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn milestone_json(m: &crate::issues_db::Milestone) -> Value {
    json!({
        "id": m.id, "title": m.title, "description": m.description, "due": m.due,
        "closed": m.closed, "open_issues": m.open_issues, "closed_issues": m.closed_issues,
    })
}

async fn milestones(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    Ok(Json(app.db.milestones(r.id)?.iter().map(milestone_json).collect::<Vec<_>>()).into_response())
}

#[derive(Deserialize)]
struct NewMilestone {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    due: Option<i64>,
    #[serde(default)]
    closed: bool,
}

async fn create_milestone(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewMilestone>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    if b.title.trim().is_empty() || b.title.chars().count() > 100 || b.description.len() > 65536 {
        return Err(bad("title 1-100 chars, description at most 64 KB"));
    }
    match app.db.create_milestone(r.id, b.title.trim(), b.description.trim(), b.due) {
        Ok(()) => {}
        Err(crate::db::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            return Err(ApiErr(StatusCode::CONFLICT, "A milestone with that title exists".into()));
        }
        Err(e) => return Err(e.into()),
    }
    let m = app.db.milestones(r.id)?.into_iter().find(|m| m.title.eq_ignore_ascii_case(b.title.trim())).ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(milestone_json(&m))).into_response())
}

async fn edit_milestone(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>, Json(b): Json<NewMilestone>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if app.db.milestones(r.id)?.iter().all(|m| m.id != id) {
        return Err(not_found());
    }
    if b.title.trim().is_empty() || b.title.chars().count() > 100 || b.description.len() > 65536 {
        return Err(bad("title 1-100 chars, description at most 64 KB"));
    }
    app.db.update_milestone(r.id, id, b.title.trim(), b.description.trim(), b.due, b.closed)?;
    let m = app.db.milestones(r.id)?.into_iter().find(|m| m.id == id).ok_or_else(not_found)?;
    Ok(Json(milestone_json(&m)).into_response())
}

async fn delete_milestone(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if app.db.milestones(r.id)?.iter().all(|m| m.id != id) {
        return Err(not_found());
    }
    app.db.delete_milestone(r.id, id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------- releases ----------

fn release_json(app: &App, r: &Repo, rel: &crate::releases::Release, assets: &[crate::releases::Asset]) -> Value {
    json!({
        "id": rel.id, "tag_name": rel.tag, "name": rel.title, "body": rel.body,
        "draft": rel.draft, "prerelease": rel.prerelease,
        "author": { "login": rel.author }, "created_at": rel.created,
        "html_url": format!("{}/{}/{}/releases/{}", app.url, r.owner, r.name, rel.id),
        "assets": assets.iter().map(|a| json!({
            "id": a.id, "name": a.name, "size": a.size, "downloads": a.downloads,
            "browser_download_url": format!("{}/{}/{}/releases/assets/{}", app.url, r.owner, r.name, a.id),
        })).collect::<Vec<_>>(),
    })
}

async fn list_releases(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let can_edit = u.as_ref().is_some_and(|u| app.authorize(Some(u), &o, &n, Access::Write).is_some());
    let mut out = Vec::new();
    for rel in app.db.releases(r.id)? {
        if rel.draft && !can_edit {
            continue;
        }
        let assets = app.db.release_assets(rel.id)?;
        out.push(release_json(&app, &r, &rel, &assets));
    }
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct NewRelease {
    tag_name: String,
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

async fn create_release(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewRelease>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    if !crate::releases::valid_tag(b.tag_name.trim()) || b.name.trim().is_empty() || b.name.chars().count() > 256 || b.body.len() > 65536 {
        return Err(bad("tag (letters, digits, . _ / + ~ -), a name, notes under 64 KB"));
    }
    let tag = b.tag_name.trim().to_owned();
    let dir = app.repo_dir(&r);
    let t = tag.clone();
    let exists = block(move || Ok(crate::gitops::rev_parse(&dir, &format!("refs/tags/{t}")).is_some())).await?;
    if !exists {
        return Err(bad("There's no such tag in this repository. Push the tag first."));
    }
    let id = app.db.create_release(r.id, &tag, b.name.trim(), &b.body, u.id, b.draft, b.prerelease)?;
    let rel = app.db.release(r.id, id)?.ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(release_json(&app, &r, &rel, &[]))).into_response())
}

async fn edit_release(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>, Json(b): Json<NewRelease>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    let Some(rel) = app.db.release(r.id, id)? else { return Err(not_found()) };
    if b.tag_name.trim() != rel.tag {
        return Err(bad("Releases are keyed by their tag; delete and recreate instead"));
    }
    if b.name.trim().is_empty() || b.name.chars().count() > 256 || b.body.len() > 65536 {
        return Err(bad("name 1-256 chars, notes under 64 KB"));
    }
    app.db.update_release(r.id, id, b.name.trim(), &b.body, b.draft, b.prerelease)?;
    let rel = app.db.release(r.id, id)?.ok_or_else(not_found)?;
    let assets = app.db.release_assets(id)?;
    Ok(Json(release_json(&app, &r, &rel, &assets)).into_response())
}

async fn delete_release(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if app.db.release(r.id, id)?.is_none() {
        return Err(not_found());
    }
    app.db.delete_release(r.id, id)?;
    let _ = std::fs::remove_dir_all(app.data.join("releases").join(r.id.to_string()).join(id.to_string()));
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------- pull requests ----------

#[derive(Deserialize)]
struct NewPull {
    base: String,
    head: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
}

async fn create_pull(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewPull>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Read)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    let (a, user, repo, dir) = (app.clone(), u.clone(), r.clone(), app.repo_dir(&r));
    let (base, head, title, body) = (b.base, b.head, b.title, b.body);
    let draft = b.draft;
    let res = block(move || {
        let f = crate::pulls_web::NewPr { base: &base, head: &head, title: &title, body: &body, draft };
        crate::pulls_web::create_pr(&a, &user, &repo, &dir, &f).map_err(|e| bad(&e))
    })
    .await?;
    let num = match res {
        crate::pulls_web::CreatedPr::Exists(n) => {
            return Err(ApiErr(StatusCode::CONFLICT, format!("Pull request #{n} already proposes these changes")));
        }
        crate::pulls_web::CreatedPr::Created(n) => n,
    };
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(issue_json(&app, &r, &i))).into_response())
}

#[derive(Deserialize)]
struct MergePull {
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    expected_head: Option<String>,
}

async fn merge_pull(State(app): St, ApiUser(u): ApiUser, Path((o, n, num)): Path<(String, String, i64)>, Json(b): Json<MergePull>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if r.archived {
        return Err(ApiErr(StatusCode::FORBIDDEN, "Repository is archived".into()));
    }
    let ctx = crate::web::Ctx::load(&app, Some(u.clone()), &o, &n)?;
    let (a, user, number) = (app.clone(), u.clone(), num);
    let f = crate::pulls_web::MergeF { strategy: b.strategy.unwrap_or_else(|| "merge".into()), expected_head: b.expected_head };
    block(move || {
        let (issue, pull) = crate::pulls_web::load_pull(&a, &ctx, number)?;
        crate::pulls_web::do_merge(&a, &ctx, &user, &issue, &pull, &f).map_err(|e| bad(&e))
    })
    .await?;
    let i = app.db.issue(r.id, num)?.ok_or_else(not_found)?;
    Ok(Json(issue_json(&app, &r, &i)).into_response())
}

// ---------- webhooks ----------

fn hook_json(app: &App, r: &Repo, h: &crate::social_db::Webhook) -> Value {
    json!({
        "id": h.id, "url": h.url, "events": h.events, "active": h.active,
        "config": { "url": h.url, "content_type": "json", "insecure_ssl": "0" },
        "html_url": format!("{}/{}/{}/settings/hooks", app.url, r.owner, r.name),
    })
}

async fn list_hooks(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Write)?;
    let hooks = app.db.webhooks(r.id)?;
    Ok(Json(hooks.iter().map(|h| hook_json(&app, &r, h)).collect::<Vec<_>>()).into_response())
}

#[derive(Deserialize)]
struct NewHook {
    url: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default)]
    active: bool,
}

async fn create_hook(State(app): St, ApiUser(u): ApiUser, Path((o, n)): Path<(String, String)>, Json(b): Json<NewHook>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if !(b.url.starts_with("http://") || b.url.starts_with("https://")) || b.url.len() > 2000 {
        return Err(bad("url must be http(s) and at most 2000 chars"));
    }
    let events = match &b.events {
        None => "*".to_owned(),
        Some(list) if list.is_empty() => "*".to_owned(),
        Some(list) => {
            for e in list {
                if !crate::social_db::HOOK_EVENTS.contains(&e.as_str()) {
                    return Err(bad(&format!("unknown event '{e}'; pick from push, issues, issue_comment, pull_request, pull_request_review, status")));
                }
            }
            if list.len() == crate::social_db::HOOK_EVENTS.len() { "*".to_owned() } else { list.join(",") }
        }
    };
    if b.secret.len() > 200 {
        return Err(bad("secret at most 200 chars"));
    }
    let id = app.db.create_webhook(r.id, &b.url, &b.secret, &events, b.active)?;
    if b.active {
        crate::events::queue_ping(&app.db, &app.url, &r, id);
    }
    let h = app.db.webhook(r.id, id)?.ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(hook_json(&app, &r, &h))).into_response())
}

async fn delete_hook(State(app): St, ApiUser(u): ApiUser, Path((o, n, id)): Path<(String, String, i64)>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if app.db.webhook(r.id, id)?.is_none() {
        return Err(not_found());
    }
    app.db.delete_webhook(r.id, id)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------- statuses ----------

#[derive(Deserialize)]
struct NewStatus {
    state: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    target_url: String,
}

fn status_json(s: &Status) -> Value {
    json!({
        "state": s.state, "context": s.context, "description": s.description,
        "target_url": s.target_url, "creator": { "login": s.creator }, "created_at": s.created,
    })
}

/// Resolve a sha/branch/tag to a full commit id in the repo.
fn commit_of(app: &App, r: &Repo, rev: &str) -> Result<String, ApiErr> {
    let dir = app.repo_dir(r);
    crate::gitops::rev_parse(&dir, rev).ok_or_else(|| ApiErr(StatusCode::UNPROCESSABLE_ENTITY, format!("No commit found for '{rev}'")))
}

async fn create_status(State(app): St, ApiUser(u): ApiUser, Path((o, n, sha)): Path<(String, String, String)>, Json(b): Json<NewStatus>) -> ApiRes {
    let u = u.ok_or(ApiErr(StatusCode::UNAUTHORIZED, "Requires authentication".into()))?;
    let r = repo_for(&app, Some(&u), &o, &n, Access::Write)?;
    if !STATES.contains(&b.state.as_str()) {
        return Err(bad("state must be one of: pending, success, failure, error"));
    }
    let context = b.context.unwrap_or_else(|| "default".into());
    if context.is_empty() || context.len() > 100 || b.description.len() > 500 || b.target_url.len() > 2000 {
        return Err(bad("context 1-100 chars, description <= 500, target_url <= 2000"));
    }
    if !b.target_url.is_empty() && !(b.target_url.starts_with("http://") || b.target_url.starts_with("https://")) {
        return Err(bad("target_url must be http(s)"));
    }
    let full = commit_of(&app, &r, &sha)?;
    let s = Status { context, state: b.state, description: b.description, target_url: b.target_url, creator: Some(u.name.clone()), created: crate::db::now() };
    app.db.add_status(r.id, &full, &s, u.id)?;
    crate::events::emit(&app.bus(), crate::events::Ev::Status { repo: &r, actor: &u, sha: &full, status: &s });
    Ok((StatusCode::CREATED, Json(status_json(&s))).into_response())
}

async fn list_statuses(State(app): St, ApiUser(u): ApiUser, Path((o, n, sha)): Path<(String, String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let full = commit_of(&app, &r, &sha)?;
    let list = app.db.statuses(r.id, &full)?;
    Ok(Json(list.iter().map(status_json).collect::<Vec<_>>()).into_response())
}

/// Combined state: failure/error beats pending beats success; none = pending.
pub fn combined(list: &[Status]) -> &'static str {
    if list.iter().any(|s| s.state == "failure" || s.state == "error") {
        "failure"
    } else if list.is_empty() || list.iter().any(|s| s.state == "pending") {
        "pending"
    } else {
        "success"
    }
}

async fn combined_status(State(app): St, ApiUser(u): ApiUser, Path((o, n, rev)): Path<(String, String, String)>) -> ApiRes {
    let r = repo_for(&app, u.as_ref(), &o, &n, Access::Read)?;
    let full = commit_of(&app, &r, &rev)?;
    let list = app.db.statuses(r.id, &full)?;
    Ok(Json(json!({
        "state": combined(&list), "sha": full, "total_count": list.len(),
        "statuses": list.iter().map(status_json).collect::<Vec<_>>(),
    }))
    .into_response())
}

async fn api_404() -> ApiErr {
    not_found()
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/user", get(me))
        .route("/api/v1/repos/{owner}/{name}", get(repo))
        .route("/api/v1/repos/{owner}/{name}/branches", get(branches))
        .route("/api/v1/repos/{owner}/{name}/issues", get(issues).post(create_issue))
        .route("/api/v1/repos/{owner}/{name}/issues/{number}", get(issue).patch(edit_issue))
        .route("/api/v1/repos/{owner}/{name}/issues/{number}/comments", get(list_comments).post(create_comment))
        .route("/api/v1/repos/{owner}/{name}/pulls", get(pulls).post(create_pull))
        .route("/api/v1/repos/{owner}/{name}/pulls/{number}", get(issue))
        .route("/api/v1/repos/{owner}/{name}/pulls/{number}/merge", post(merge_pull))
        .route("/api/v1/repos/{owner}/{name}/labels", get(labels).post(create_label))
        .route("/api/v1/repos/{owner}/{name}/labels/{id}", axum::routing::patch(edit_label).delete(delete_label))
        .route("/api/v1/repos/{owner}/{name}/milestones", get(milestones).post(create_milestone))
        .route("/api/v1/repos/{owner}/{name}/milestones/{id}", axum::routing::patch(edit_milestone).delete(delete_milestone))
        .route("/api/v1/repos/{owner}/{name}/releases", get(list_releases).post(create_release))
        .route("/api/v1/repos/{owner}/{name}/releases/{id}", axum::routing::patch(edit_release).delete(delete_release))
        .route("/api/v1/repos/{owner}/{name}/hooks", get(list_hooks).post(create_hook))
        .route("/api/v1/repos/{owner}/{name}/hooks/{id}", axum::routing::delete(delete_hook))
        .route("/api/v1/repos/{owner}/{name}/statuses/{sha}", get(list_statuses).post(create_status))
        .route("/api/v1/repos/{owner}/{name}/commits/{rev}/status", get(combined_status))
        .route("/api/v1/{*rest}", get(api_404).post(api_404).patch(api_404).delete(api_404))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn st(s: &str) -> Status {
        Status { context: "c".into(), state: s.into(), description: String::new(), target_url: String::new(), creator: None, created: 0 }
    }
    #[test]
    fn combined_state() {
        assert_eq!(combined(&[]), "pending");
        assert_eq!(combined(&[st("success"), st("success")]), "success");
        assert_eq!(combined(&[st("success"), st("pending")]), "pending");
        assert_eq!(combined(&[st("pending"), st("error")]), "failure");
    }
}
