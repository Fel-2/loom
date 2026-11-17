//! Routes: notifications, watching, search, webhooks.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use maud::html;
use serde::Deserialize;

use crate::app::App;
use crate::db::Access;
use crate::ui::{self, Tab};
use crate::ui_social::{self as uis, CodeHit};
use crate::web::{blocking, see_other, Authed, Ctx, Err, Res, St, Viewer};

// ---------- notifications ----------

#[derive(Deserialize)]
struct NotifQ {
    all: Option<String>,
}

async fn notifications(State(app): St, Authed(u): Authed, Query(q): Query<NotifQ>) -> Res {
    let all = q.all.is_some();
    let list = app.db.notifications(u.id, all)?;
    Ok(ui::page("notifications", Some(&u), uis::notifications(&list, all)).into_response())
}

async fn read_all(State(app): St, Authed(u): Authed) -> Res {
    app.db.mark_all_read(u.id)?;
    Ok(see_other("/notifications"))
}

async fn watch(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    app.db.toggle_watch(u.id, ctx.repo.id)?;
    Ok(see_other(&ctx.base()))
}

// ---------- search ----------

#[derive(Deserialize, Default)]
struct SearchQ {
    q: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

const MAX_Q: usize = 200;

async fn search(State(app): St, Viewer(v): Viewer, Query(sq): Query<SearchQ>) -> Res {
    let q: String = sq.q.unwrap_or_default().chars().take(MAX_Q).collect();
    let kind = match sq.kind.as_deref() {
        Some("issues") => "issues",
        _ => "repos",
    };
    let (repos, issues) = if q.trim().is_empty() {
        (Vec::new(), Vec::new())
    } else if kind == "repos" {
        (app.db.search_repos(v.as_ref(), &q, 50)?, Vec::new())
    } else {
        (Vec::new(), app.db.search_issues(v.as_ref(), &q, None, 50)?)
    };
    let d = uis::SearchData { q: &q, kind, scope: None, repos: &repos, issues: &issues, code: &[], code_rev: "", err: None };
    Ok(ui::page("search", v.as_ref(), uis::search(&d)).into_response())
}

async fn repo_search(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>, Query(sq): Query<SearchQ>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let q: String = sq.q.unwrap_or_default().chars().take(MAX_Q).collect();
        let kind = if sq.kind.as_deref() == Some("issues") { "issues" } else { "code" };
        let scope = format!("{}/{}", ctx.repo.owner, ctx.repo.name);
        let mut issues = Vec::new();
        let mut code = Vec::new();
        let mut rev = String::new();
        if !q.trim().is_empty() {
            if kind == "issues" {
                issues = app.db.search_issues(ctx.viewer.as_ref(), &q, Some(ctx.repo.id), 50)?;
            } else if let Some(head) = crate::gitops::rev_parse(&ctx.dir, "HEAD") {
                code = crate::gitops::grep(&ctx.dir, &head, q.trim(), 100)
                    .into_iter()
                    .map(|(path, line, text)| CodeHit { path, line, text })
                    .collect();
                rev = head;
            }
        }
        let d = uis::SearchData { q: &q, kind, scope: Some(&scope), repos: &[], issues: &issues, code: &code, code_rev: &rev, err: None };
        let body = html! { (ctx.header(Tab::Overview, "HEAD")) (uis::search(&d)) };
        Ok(ctx.page("search", body))
    })
    .await
}

// ---------- webhooks (repo admins) ----------

fn admin(app: &Arc<App>, u: crate::db::User, owner: &str, name: &str) -> Result<Ctx, Err> {
    Ctx::load_with(app, Some(u), owner, name, Access::Admin)
}

fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://")) && u.len() <= 2000 && !u.chars().any(char::is_whitespace)
}

async fn hook_create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, body: Bytes) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let pairs = crate::issues_web::form_pairs(&body);
    let get = |k: &str| pairs.iter().find(|(x, _)| x == k).map(|(_, v)| v.trim().to_owned()).unwrap_or_default();
    let url = get("url");
    let secret = get("secret");
    let events: Vec<&str> = pairs
        .iter()
        .filter(|(k, _)| k == "event")
        .filter_map(|(_, v)| crate::social_db::HOOK_EVENTS.iter().copied().find(|e| e == v))
        .collect();
    if !valid_url(&url) {
        return crate::web::settings_page(&app, &ctx, None, Some("Webhook URL must be http(s)://…"));
    }
    if events.is_empty() {
        return crate::web::settings_page(&app, &ctx, None, Some("Pick at least one event."));
    }
    let ev = if events.len() == crate::social_db::HOOK_EVENTS.len() { "*".to_owned() } else { events.join(",") };
    let id = app.db.create_webhook(ctx.repo.id, &url, &secret, &ev, true)?;
    crate::events::queue_ping(&app.db, &app.url, &ctx.repo, id);
    crate::web::settings_page(&app, &ctx, Some("Webhook added; a ping is on its way."), None)
}

async fn hook_delete(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    app.db.delete_webhook(ctx.repo.id, id)?;
    crate::web::settings_page(&app, &ctx, Some("Webhook deleted."), None)
}

async fn hook_page(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    hook_response(&app, &ctx, id, None)
}

fn hook_response(app: &App, ctx: &Ctx, id: i64, msg: Option<&str>) -> Res {
    let h = app.db.webhook(ctx.repo.id, id)?.ok_or(Err::NotFound)?;
    let ds = app.db.deliveries(h.id)?;
    let sbase = format!("{}/settings", ctx.base());
    let body = html! { (ctx.header(Tab::Settings, "HEAD")) (uis::hook_page(&sbase, &h, &ds, msg)) };
    Ok(ctx.page("webhook", body))
}

async fn hook_test(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let h = app.db.webhook(ctx.repo.id, id)?.ok_or(Err::NotFound)?;
    crate::events::queue_ping(&app.db, &app.url, &ctx.repo, h.id);
    Ok(see_other(&format!("{}/settings/hooks/{}", ctx.base(), h.id)))
}

async fn redeliver(State(app): St, Authed(u): Authed, Path((owner, name, id, did)): Path<(String, String, i64, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let h = app.db.webhook(ctx.repo.id, id)?.ok_or(Err::NotFound)?;
    if !app.db.redeliver(h.id, did)? {
        return Err(Err::NotFound);
    }
    crate::events::queue_ping_wake();
    Ok(see_other(&format!("{}/settings/hooks/{}", ctx.base(), h.id)))
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/notifications", get(notifications))
        .route("/notifications/read", post(read_all))
        .route("/search", get(search))
        .route("/{owner}/{name}/search", get(repo_search))
        .route("/{owner}/{name}/watch", post(watch))
        .route("/{owner}/{name}/settings/hooks", post(hook_create))
        .route("/{owner}/{name}/settings/hooks/{id}", get(hook_page))
        .route("/{owner}/{name}/settings/hooks/{id}/delete", post(hook_delete))
        .route("/{owner}/{name}/settings/hooks/{id}/test", post(hook_test))
        .route("/{owner}/{name}/settings/hooks/{id}/deliveries/{did}/redeliver", post(redeliver))
}
