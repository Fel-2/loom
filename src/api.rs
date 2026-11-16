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
        "pull_request": i.is_pr, "merged": i.merged,
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
        .route("/api/v1/repos/{owner}/{name}/issues/{number}", get(issue))
        .route("/api/v1/repos/{owner}/{name}/issues/{number}/comments", post(create_comment))
        .route("/api/v1/repos/{owner}/{name}/pulls", get(pulls))
        .route("/api/v1/repos/{owner}/{name}/pulls/{number}", get(issue))
        .route("/api/v1/repos/{owner}/{name}/statuses/{sha}", get(list_statuses).post(create_status))
        .route("/api/v1/repos/{owner}/{name}/commits/{rev}/status", get(combined_status))
        .route("/api/v1/{*rest}", get(api_404).post(api_404))
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
