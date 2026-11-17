//! Atom feeds: commits, issues and releases per repository. Server-rendered
//! XML, ETagged by what the feed is keyed on. Private repos authenticate
//! with a session cookie or an API token (readers do Basic with the token).

use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::app::App;
use crate::db::{Access, User};
use crate::web::{blocking, Ctx, Res, St};

const PER_FEED: usize = 30;

/// Session cookie first, `Authorization: Bearer loom_…` (or Basic with the
/// token as password) second, so feed readers can follow private repos.
/// A present-but-invalid token is a 401, never a silent downgrade.
pub struct FeedViewer(pub Option<User>);

impl FromRequestParts<Arc<App>> for FeedViewer {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        if let Some(u) = crate::web::Viewer::from_request_parts(parts, app).await.unwrap_or(crate::web::Viewer(None)).0 {
            return Ok(FeedViewer(Some(u)));
        }
        let Some(v) = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
            return Ok(FeedViewer(None));
        };
        let token = if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("token ")) {
            t.trim().to_owned()
        } else if let Some(b) = v.strip_prefix("Basic ") {
            use base64::Engine;
            let raw = base64::engine::general_purpose::STANDARD.decode(b.trim()).unwrap_or_default();
            String::from_utf8_lossy(&raw).split_once(':').map(|(_, p)| p.to_owned()).unwrap_or_default()
        } else {
            String::new()
        };
        match app.db.token_user(&token) {
            Ok(Some(u)) if token.starts_with("loom_") => Ok(FeedViewer(Some(u))),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

fn esc(s: &str) -> String {
    s.bytes().fold(String::with_capacity(s.len()), |mut o, b| {
        match b {
            b'&' => o.push_str("&amp;"),
            b'<' => o.push_str("&lt;"),
            b'>' => o.push_str("&gt;"),
            b'"' => o.push_str("&quot;"),
            b'\'' => o.push_str("&#39;"),
            _ => o.push(b as char),
        }
        o
    })
}

/// RFC 3339 (UTC) from Unix seconds. Feed readers want one timestamp format.
fn rfc3339(t: i64) -> String {
    let (secs, days) = (t.rem_euclid(86400), t.div_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

struct Entry {
    id: String,
    title: String,
    updated: i64,
    link: String,
    author: String,
    content: String,
}

fn feed(app: &App, ctx: &Ctx, title: &str, path: &str, updated: i64, entries: &[Entry]) -> String {
    let url = format!("{}/{}/{}", app.url, ctx.repo.owner, ctx.repo.name);
    let mut s = String::with_capacity(1024 + entries.len() * 256);
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    s.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    s.push_str(&format!("  <title>{}</title>\n", esc(title)));
    s.push_str(&format!("  <id>{}</id>\n", esc(&format!("{url}/{path}"))));
    s.push_str(&format!("  <updated>{}</updated>\n", rfc3339(updated)));
    s.push_str(&format!("  <link rel=\"self\" href=\"{}\"/>\n", esc(&format!("{url}/{path}"))));
    s.push_str(&format!("  <link rel=\"alternate\" href=\"{}\"/>\n", esc(&url)));
    for e in entries {
        s.push_str("  <entry>\n");
        s.push_str(&format!("    <id>{}</id>\n", esc(&e.id)));
        s.push_str(&format!("    <title>{}</title>\n", esc(&e.title)));
        s.push_str(&format!("    <updated>{}</updated>\n", rfc3339(e.updated)));
        s.push_str(&format!("    <link rel=\"alternate\" href=\"{}\"/>\n", esc(&e.link)));
        if !e.author.is_empty() {
            s.push_str(&format!("    <author><name>{}</name></author>\n", esc(&e.author)));
        }
        if !e.content.is_empty() {
            s.push_str(&format!("    <content type=\"text\">{}</content>\n", esc(&e.content)));
        }
        s.push_str("  </entry>\n");
    }
    s.push_str("</feed>\n");
    s
}

fn respond(headers: &HeaderMap, ctx: &Ctx, tag: &str, body: String) -> Response {    let tag = format!("\"{tag}\"");
    let hit = headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == tag.as_bytes());
    let mut res = if hit {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")], body).into_response()
    };
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    h.insert(
        header::CACHE_CONTROL,
        if ctx.repo.private {
            HeaderValue::from_static("private, no-cache")
        } else {
            HeaderValue::from_static("public, no-cache")
        },
    );
    res
}

async fn commits(State(app): St, FeedViewer(v): FeedViewer, Path((owner, name)): Path<(String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let repo = ctx.open()?;
        let base = format!("{}/{}", ctx.repo.owner, ctx.repo.name);
        let Some(head) = crate::git::head_commit(&repo) else {
            let body = feed(&app, &ctx, &format!("{base}: commits"), "atom.xml", crate::db::now(), &[]);
            return Ok(respond(&headers, &ctx, "atom-empty", body));
        };
        let commits = crate::git::log(&repo, head, "", 0, PER_FEED).unwrap_or_default();
        let entries: Vec<Entry> = commits
            .iter()
            .map(|c| Entry {
                id: format!("{}/{base}/commit/{id}", app.url, id = c.id),
                title: c.summary.clone(),
                updated: c.time,
                link: format!("{}/{base}/commit/{}", app.url, c.id),
                author: c.author.clone(),
                content: c.message.lines().skip(1).collect::<Vec<_>>().join("\n").trim().to_owned(),
            })
            .collect();
        let updated = commits.first().map_or(crate::db::now(), |c| c.time);
        let body = feed(&app, &ctx, &format!("{base}: commits"), "atom.xml", updated, &entries);
        Ok(respond(&headers, &ctx, &format!("atom-{head}"), body))
    })
    .await
}

async fn issues(State(app): St, FeedViewer(v): FeedViewer, Path((owner, name)): Path<(String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let list = app.db.recent_issues(ctx.repo.id, PER_FEED)?;
        let entries: Vec<Entry> = list
            .iter()
            .map(|i| Entry {
                id: format!("{}/{}/{}/{}/{}", app.url, ctx.repo.owner, ctx.repo.name, if i.is_pr { "pulls" } else { "issues" }, i.number),
                title: format!("#{} {} [{}]", i.number, i.title, if i.merged { "merged" } else if i.closed { "closed" } else { "open" }),
                updated: i.updated,
                link: format!("{}/{}/{}/{}/{}", app.url, ctx.repo.owner, ctx.repo.name, if i.is_pr { "pulls" } else { "issues" }, i.number),
                author: i.author.clone().unwrap_or_default(),
                content: i.body.clone(),
            })
            .collect();
        let updated = list.first().map_or(crate::db::now(), |i| i.updated);
        let title = format!("{}/{}: issues", ctx.repo.owner, ctx.repo.name);
        Ok(respond(&headers, &ctx, &format!("atom-issues-{updated}-{}", list.len()), feed(&app, &ctx, &title, "issues.atom.xml", updated, &entries)))
    })
    .await
}

async fn releases(State(app): St, FeedViewer(v): FeedViewer, Path((owner, name)): Path<(String, String)>, headers: HeaderMap) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let can_edit = ctx.access >= Access::Write;
        let all: Vec<_> = app
            .db
            .releases(ctx.repo.id)?
            .into_iter()
            .filter(|r| can_edit || !r.draft)
            .take(PER_FEED)
            .collect();
        let entries: Vec<Entry> = all
            .iter()
            .map(|r| Entry {
                id: format!("{}/{}/{}/releases/{}", app.url, ctx.repo.owner, ctx.repo.name, r.id),
                title: format!("{} ({})", r.title, r.tag),
                updated: r.created,
                link: format!("{}/{}/{}/releases/{}", app.url, ctx.repo.owner, ctx.repo.name, r.id),
                author: r.author.clone().unwrap_or_default(),
                content: r.body.clone(),
            })
            .collect();
        let updated = all.first().map_or(crate::db::now(), |r| r.created);
        let title = format!("{}/{}: releases", ctx.repo.owner, ctx.repo.name);
        Ok(respond(&headers, &ctx, &format!("atom-rel-{updated}-{}", all.len()), feed(&app, &ctx, &title, "releases.atom.xml", updated, &entries)))
    })
    .await
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/atom.xml", get(commits))
        .route("/{owner}/{name}/issues.atom.xml", get(issues))
        .route("/{owner}/{name}/releases.atom.xml", get(releases))
}
