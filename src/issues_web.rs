//! Issue routes: list, create, view, comment, triage, labels, milestones.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;

use crate::app::App;
use crate::db::{Access, Repo, User};
use crate::issues_db::{self, Filter, Issue};
use crate::refs::Token;
use crate::git;
use crate::ui::{self, RefLink, Resolve};
use crate::ui_issues::{self as uii, Item};
use crate::web::{blocking, see_other, Authed, Ctx, Err, Res, St, Viewer};

const MAX_TITLE: usize = 256;
const MAX_BODY: usize = 65536;
const PER_PAGE: usize = 30;

// ---------- references ----------

/// Links `#N`, `owner/repo#N`, `@user` and shas — only to things the viewer may see.
pub struct Refs<'a> {
    pub app: &'a App,
    pub viewer: Option<&'a User>,
    pub repo: &'a Repo,
    pub git: Option<gix::Repository>,
}

impl Resolve for Refs<'_> {
    fn link(&self, t: &Token) -> Option<RefLink> {
        match t {
            Token::Issue { repo: None, number, .. } => {
                let (title, closed, is_pr) = self.app.db.issue_brief(self.repo.id, *number).ok()??;
                let kind = if is_pr { "pulls" } else { "issues" };
                Some(RefLink {
                    href: format!("/{}/{}/{kind}/{number}", self.repo.owner, self.repo.name),
                    title,
                    class: if closed { "ref ref-closed" } else { "ref ref-open" },
                })
            }
            Token::Issue { repo: Some((o, n)), number, .. } => {
                let (r, _) = self.app.authorize(self.viewer, o, n, Access::Read)?;
                let (title, closed, is_pr) = self.app.db.issue_brief(r.id, *number).ok()??;
                let kind = if is_pr { "pulls" } else { "issues" };
                Some(RefLink {
                    href: format!("/{}/{}/{kind}/{number}", r.owner, r.name),
                    title,
                    class: if closed { "ref ref-closed" } else { "ref ref-open" },
                })
            }
            Token::User(name) => {
                let o = self.app.db.owner(name).ok()??;
                Some(RefLink { href: format!("/{}", o.name), title: o.name, class: "ref mention" })
            }
            Token::Sha(hex) => {
                let repo = self.git.as_ref()?;
                let id = crate::git::resolve_commit(repo, hex).ok()?;
                let c = crate::git::commit(repo, id).ok()?;
                Some(RefLink {
                    href: format!("/{}/{}/commit/{id}", self.repo.owner, self.repo.name),
                    title: c.summary,
                    class: "ref sha",
                })
            }
        }
    }
}

/// Record "referenced from" events on issues mentioned in `text` (written by
/// `author` in `src`). The author must be able to read the target.
pub fn record_mentions(app: &App, author: &User, src: &Repo, src_issue: &Issue, text: &str) {
    for (repo, number, _) in crate::refs::issue_refs(text) {
        let target = match &repo {
            None => Some(src.clone()),
            Some((o, n)) => app.authorize(Some(author), o, n, Access::Read).map(|(r, _)| r),
        };
        let Some(target) = target else { continue };
        if target.id == src.id && number == src_issue.number {
            continue;
        }
        let Ok(Some(tid)) = app.db.issue_id(target.id, number) else { continue };
        let key = format!("iref:{}:{tid}", src_issue.id);
        let _ = app.db.add_event(tid, Some(author.id), "referenced", &format!("{}\t{}", src.id, src_issue.number), Some(&key));
    }
}

/// Timeline text for an event. Cross-repo sources stay hidden unless visible.
fn event_text(app: &App, viewer: Option<&User>, repo: &Repo, number: i64, e: &issues_db::Event) -> Option<maud::Markup> {
    let base = format!("/{}/{}", repo.owner, repo.name);
    let (a, b) = e.data.split_once('\t').unwrap_or((&e.data, ""));
    Some(match e.kind.as_str() {
        "closed" => html! { "closed this" },
        "reopened" => html! { "reopened this" },
        "renamed" => html! { "changed the title " del { (a) } " → " strong { (b) } },
        "labeled" => html! { "added label " strong { (a) } },
        "unlabeled" => html! { "removed label " strong { (a) } },
        "assigned" => html! { "assigned " a href={ "/" (a) } { (a) } },
        "unassigned" => html! { "unassigned " a href={ "/" (a) } { (a) } },
        "milestoned" => html! { "added this to " strong { (a) } },
        "demilestoned" => html! { "removed this from " strong { (a) } },
        "commit_ref" | "commit_closed" => {
            let short = &a[..a.len().min(8)];
            let verb = if e.kind == "commit_closed" { "closed this in" } else { "referenced this in" };
            html! { (verb) " commit " a.sha href={ (base) "/commit/" (a) } { (short) } " " span.muted { (b) } }
        }
        "pushed" => {
            let mut it = e.data.split('\t');
            let (n, sha, forced, count) = (it.next()?, it.next()?, it.next().unwrap_or(""), it.next().unwrap_or("0"));
            html! {
                @if forced == "force" { "force-pushed " } @else { "pushed " (count) " commit" @if count != "1" { "s" } " " }
                a href={ (base) "/pulls/" (number) "/revisions" } { "v" (n) }
                " " a.sha href={ (base) "/commit/" (sha) } { (&sha[..sha.len().min(8)]) }
            }
        }
        "merged" => {
            let mut it = e.data.split('\t');
            let (sha, branch) = (it.next()?, it.next().unwrap_or(""));
            html! { "merged commit " a.sha href={ (base) "/commit/" (sha) } { (&sha[..sha.len().min(8)]) } " into " code { (branch) } }
        }
        "head_deleted" => html! { "deleted the head branch " code { (a) } },
        "pr_closed" => html! { "closed this via " a href={ (base) "/pulls/" (a) } { "#" (a) } },
        "referenced" => {
            let (src_id, num): (i64, i64) = (a.parse().ok()?, b.parse().ok()?);
            let src = app.db.repo_by_id(src_id).ok()??;
            if app.db.access(viewer, &src).ok()? < Access::Read {
                return None;
            }
            let (title, _, is_pr) = app.db.issue_brief(src.id, num).ok()??;
            let label = if src.id == repo.id { format!("#{num}") } else { format!("{}/{}#{num}", src.owner, src.name) };
            let kind = if is_pr { "pulls" } else { "issues" };
            html! { "mentioned this in " a href={ "/" (src.owner) "/" (src.name) "/" (kind) "/" (num) } { (label) } " " span.muted { (title) } }
        }
        _ => return None,
    })
}

// ---------- helpers ----------

fn can_triage(ctx: &Ctx) -> bool {
    ctx.access >= Access::Write
}

/// Issue or PR by number (they share numbering and these handlers).
fn load_issue(app: &App, ctx: &Ctx, number: i64) -> Result<Issue, Err> {
    app.db.issue(ctx.repo.id, number)?.ok_or(Err::NotFound)
}

pub fn kind_of(i: &Issue) -> &'static str {
    if i.is_pr { "pulls" } else { "issues" }
}

fn back(ctx: &Ctx, i: &Issue) -> Response {
    see_other(&format!("{}/{}/{}", ctx.base(), kind_of(i), i.number))
}

/// Parse an urlencoded body allowing repeated keys.
pub fn form_pairs(body: &[u8]) -> Vec<(String, String)> {
    fn dec(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < b.len() => {
                    let hex = |c: u8| (c as char).to_digit(16);
                    match (hex(b[i + 1]), hex(b[i + 2])) {
                        (Some(h), Some(l)) => {
                            out.push((h << 4 | l) as u8);
                            i += 2;
                        }
                        _ => out.push(b'%'),
                    }
                }
                c => out.push(c),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    std::str::from_utf8(body)
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (dec(k), dec(v))
        })
        .collect()
}

fn check_text(title: Option<&str>, body: &str) -> Result<(), &'static str> {
    if let Some(t) = title {
        if t.trim().is_empty() {
            return Err("A title is required.");
        }
        if t.chars().count() > MAX_TITLE {
            return Err("The title is too long (256 characters max).");
        }
    }
    if body.len() > MAX_BODY {
        return Err("The text is too long (64 KB max).");
    }
    Ok(())
}

// ---------- list / new ----------

#[derive(Deserialize)]
struct ListQ {
    state: Option<String>,
    label: Option<String>,
    milestone: Option<String>,
    assignee: Option<String>,
    author: Option<String>,
    page: Option<usize>,
}

async fn list(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>, Query(q): Query<ListQ>) -> Res {
    list_impl(&app, v, &owner, &name, q, false)
}

async fn list_pulls(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>, Query(q): Query<ListQ>) -> Res {
    list_impl(&app, v, &owner, &name, q, true)
}

fn list_impl(app: &Arc<App>, v: Option<User>, owner: &str, name: &str, q: ListQ, is_pr: bool) -> Res {
    let ctx = Ctx::load(app, v, owner, name)?;
    let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
    let f = Filter {
        closed: q.state.as_deref() == Some("closed"),
        label: nonempty(q.label),
        milestone: nonempty(q.milestone),
        assignee: nonempty(q.assignee),
        author: nonempty(q.author),
    };
    let page = q.page.unwrap_or(0).min(10_000);
    let mut issues = app.db.list_issues(ctx.repo.id, is_pr, &f, PER_PAGE + 1, page * PER_PAGE)?;
    let more = issues.len() > PER_PAGE;
    issues.truncate(PER_PAGE);
    let (open, closed) = app.db.issue_counts(ctx.repo.id, is_pr, &f)?;
    let base = ctx.base();
    let kind = if is_pr { "pulls" } else { "issues" };
    let d = uii::ListData {
        kind,
        base: &base,
        issues: &issues,
        filter: &f,
        open,
        closed,
        page,
        more,
        can_create: ctx.viewer.is_some() && !ctx.repo.archived,
    };
    let tab = if is_pr { ui::Tab::Pulls } else { ui::Tab::Issues };
    let body = html! { (ctx.header(tab, "HEAD")) (uii::list(&d)) };
    Ok(ctx.page(kind, body))
}

async fn new_page(
    State(app): St,
    Authed(u): Authed,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<TemplateQ>,
) -> Res {
    let ctx = Ctx::load(&app, Some(u), &owner, &name)?;
    if ctx.repo.archived {
        return Err(Err::NotFound);
    }
    blocking(move || {
        let repo = ctx.open()?;
        let tpls = git::head_commit(&repo).map(|h| git::templates(&repo, h, "issues")).unwrap_or_default();
        // A template is code from the default branch: shown as text, never executed.
        let body = q
            .template
            .as_deref()
            .and_then(|t| tpls.iter().find(|(n, _)| n == t).map(|(_, b)| b.clone()))
            .unwrap_or_default();
        let picker = (!tpls.is_empty()).then(|| {
            html! {
                div.issue-bar {
                    span.muted.small { "templates:" }
                    @for (n, _) in &tpls { a.filter-chip href={ (ctx.base()) "/issues/new?template=" (crate::web::urlencode(n)) } { (n) } }
                }
            }
        });
        let page = html! {
            (ctx.header(ui::Tab::Issues, "HEAD"))
            @if let Some(p) = picker { (p) }
            (uii::new_issue(&ctx.base(), "", &body, None))
        };
        Ok(ctx.page("new issue", page))
    })
    .await
}

#[derive(Deserialize, Default)]
pub struct TemplateQ {
    template: Option<String>,
}

#[derive(Deserialize)]
struct NewF {
    title: String,
    body: String,
}

async fn create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<NewF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    if ctx.repo.archived {
        return Err(Err::NotFound);
    }
    if let Err(e) = check_text(Some(&f.title), &f.body) {
        let body = html! { (ctx.header(ui::Tab::Issues, "HEAD")) (uii::new_issue(&ctx.base(), &f.title, &f.body, Some(e))) };
        return Ok((StatusCode::BAD_REQUEST, ctx.page("new issue", body)).into_response());
    }
    let (_, number) = app.db.create_issue(ctx.repo.id, u.id, f.title.trim(), &f.body, false)?;
    if let Some(issue) = app.db.issue(ctx.repo.id, number)? {
        record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
        crate::events::emit(&app.bus(), crate::events::Ev::Issue { repo: &ctx.repo, actor: &u, issue: &issue, action: "opened" });
    }
    Ok(see_other(&format!("{}/issues/{number}", ctx.base())))
}

// ---------- view (issues; PRs render in pulls_web) ----------

pub fn render_issue(app: &Arc<App>, ctx: &Ctx, number: i64, msg: Option<&str>, err: Option<&str>) -> Res {
    let issue = load_issue(app, ctx, number)?;
    if let Some(u) = &ctx.viewer {
        let _ = app.db.mark_read(u.id, issue.id);
    }
    if issue.is_pr {
        return crate::pulls_web::render_pull(app, ctx, issue, msg, err);
    }
    let parts = issue_parts(app, ctx, &issue)?;
    let base = ctx.base();
    let d = show_data(ctx, &base, &issue, parts, msg, err);
    let body = html! { (ctx.header(ui::Tab::Issues, "HEAD")) (uii::show(&d)) };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page(&format!("{} #{}", issue.title, issue.number), body)).into_response())
}

/// Everything the conversation view needs, shared by issues and PRs.
pub struct Parts {
    pub items: Vec<(i64, Item)>,
    pub body_html: maud::Markup,
    pub assignees: Vec<String>,
    pub labels: Vec<issues_db::Label>,
    pub milestones: Vec<issues_db::Milestone>,
}

pub fn issue_parts(app: &Arc<App>, ctx: &Ctx, issue: &Issue) -> Result<Parts, Err> {
    let viewer = ctx.viewer.as_ref();
    let refs = Refs { app, viewer, repo: &ctx.repo, git: crate::git::open(&ctx.dir).ok() };
    let mut items: Vec<(i64, Item)> = Vec::new();
    for c in app.db.comments(issue.id)? {
        let mine = viewer.is_some_and(|u| Some(u.id) == c.author_id);
        let html = ui::markdown_refs(&c.body, &refs);
        let can_delete = mine || ctx.access >= Access::Admin;
        items.push((c.created, Item::Comment { can_edit: mine && !ctx.repo.archived, can_delete, html, c }));
    }
    for e in app.db.events(issue.id)? {
        if let Some(text) = event_text(app, viewer, &ctx.repo, issue.number, &e) {
            items.push((e.created, Item::Event { actor: e.actor.clone(), created: e.created, text }));
        }
    }
    Ok(Parts {
        items,
        body_html: ui::markdown_refs(&issue.body, &refs),
        assignees: app.db.assignees(issue.id)?,
        labels: app.db.labels(ctx.repo.id)?,
        milestones: app.db.milestones(ctx.repo.id)?,
    })
}

pub fn show_data<'a>(ctx: &Ctx, base: &'a str, issue: &'a Issue, mut parts: Parts, msg: Option<&'a str>, err: Option<&'a str>) -> uii::ShowData<'a> {
    let viewer = ctx.viewer.as_ref();
    let is_author = viewer.is_some_and(|u| Some(u.id) == issue.author_id);
    let triage = can_triage(ctx);
    // Stable: same-second items keep insertion order.
    parts.items.sort_by_key(|(t, _)| *t);
    let locked = ctx.repo.archived || issue.merged;
    uii::ShowData {
        kind: kind_of(issue),
        base,
        issue,
        body_html: parts.body_html,
        items: parts.items.into_iter().map(|(_, i)| i).collect(),
        assignees: parts.assignees,
        labels: parts.labels,
        milestones: parts.milestones,
        signed_in: viewer.is_some(),
        can_comment: viewer.is_some() && !ctx.repo.archived,
        can_edit: (is_author || triage) && !ctx.repo.archived,
        can_triage: triage && !ctx.repo.archived,
        can_close: (is_author || triage) && !locked,
        archived: ctx.repo.archived,
        msg,
        err,
        head: None,
        merge_box: None,
        extra_side: None,
    }
}

async fn show(State(app): St, Viewer(v): Viewer, Path((owner, name, number)): Path<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    render_issue(&app, &ctx, number, None, None)
}

// ---------- mutate ----------

#[derive(Deserialize)]
struct CommentF {
    body: String,
    action: Option<String>,
}

async fn comment(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<CommentF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let issue = load_issue(&app, &ctx, number)?;
    if ctx.repo.archived {
        return render_issue(&app, &ctx, number, None, Some("This repository is archived."));
    }
    if let Err(e) = check_text(None, &f.body) {
        return render_issue(&app, &ctx, number, None, Some(e));
    }
    let action = f.action.as_deref().unwrap_or("comment");
    if action != "comment" && issue.merged {
        return render_issue(&app, &ctx, number, None, Some("This pull request is merged."));
    }
    let can_close = Some(u.id) == issue.author_id || can_triage(&ctx);
    if action != "comment" && !can_close {
        return render_issue(&app, &ctx, number, None, Some("You can't change this issue's state."));
    }
    if !f.body.trim().is_empty() {
        app.db.add_comment(issue.id, u.id, &f.body)?;
        record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
        crate::events::emit(&app.bus(), crate::events::Ev::Comment { repo: &ctx.repo, actor: &u, issue: &issue, body: &f.body });
    } else if action == "comment" {
        return render_issue(&app, &ctx, number, None, Some("The comment is empty."));
    }
    let changed = match action {
        "close" => app.db.set_issue_closed(issue.id, true, Some(u.id), "closed", "", None)?,
        "reopen" => app.db.set_issue_closed(issue.id, false, Some(u.id), "reopened", "", None)?,
        _ => false,
    };
    if changed {
        let ev_action = if action == "close" { "closed" } else { "reopened" };
        if let Some(fresh) = app.db.issue(ctx.repo.id, number)? {
            match app.db.pull(ctx.repo.id, number)? {
                Some(p) => crate::events::emit(&app.bus(), crate::events::Ev::Pull { repo: &ctx.repo, actor: Some(&u), issue: &fresh, pull: &p, action: ev_action }),
                None => crate::events::emit(&app.bus(), crate::events::Ev::Issue { repo: &ctx.repo, actor: &u, issue: &fresh, action: ev_action }),
            }
        }
    }
    Ok(back(&ctx, &issue))
}

async fn edit(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<NewF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let issue = load_issue(&app, &ctx, number)?;
    if ctx.repo.archived || !(Some(u.id) == issue.author_id || can_triage(&ctx)) {
        return render_issue(&app, &ctx, number, None, Some("You can't edit this issue."));
    }
    if let Err(e) = check_text(Some(&f.title), &f.body) {
        return render_issue(&app, &ctx, number, None, Some(e));
    }
    app.db.edit_issue(&issue, u.id, f.title.trim(), &f.body)?;
    if f.body != issue.body {
        record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
    }
    Ok(back(&ctx, &issue))
}

/// Triage actions need write access; others get the issue page with an error.
fn triage_ctx(app: &Arc<App>, u: User, owner: &str, name: &str) -> Result<Ctx, Err> {
    let ctx = Ctx::load(app, Some(u), owner, name)?;
    if !can_triage(&ctx) || ctx.repo.archived {
        return Err(Err::NotFound);
    }
    Ok(ctx)
}

async fn set_labels(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, body: Bytes) -> Res {
    let ctx = triage_ctx(&app, u.clone(), &owner, &name)?;
    let issue = load_issue(&app, &ctx, number)?;
    let ids: Vec<i64> = form_pairs(&body).into_iter().filter(|(k, _)| k == "label").filter_map(|(_, v)| v.parse().ok()).collect();
    app.db.set_issue_labels(ctx.repo.id, issue.id, &ids, u.id)?;
    Ok(back(&ctx, &issue))
}

#[derive(Deserialize)]
struct AssignF {
    add: Option<String>,
    remove: Option<String>,
}

async fn assign(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<AssignF>) -> Res {
    let ctx = triage_ctx(&app, u.clone(), &owner, &name)?;
    let issue = load_issue(&app, &ctx, number)?;
    if let Some(n) = f.add.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let Some(t) = app.db.user_by_name(n)? else {
            return render_issue(&app, &ctx, number, None, Some(&format!("No user named '{n}'.")));
        };
        if app.db.access(Some(&t), &ctx.repo)? < Access::Read {
            return render_issue(&app, &ctx, number, None, Some(&format!("{} can't see this repository.", t.name)));
        }
        app.db.add_assignee(issue.id, t.id, &t.name, u.id)?;
        if t.id != u.id {
            let _ = app.db.notify(t.id, ctx.repo.id, issue.id, "assigned", Some(u.id));
        }
    }
    if let Some(n) = f.remove.as_deref() {
        app.db.remove_assignee(issue.id, n, u.id)?;
    }
    Ok(back(&ctx, &issue))
}

#[derive(Deserialize)]
struct MilestoneF {
    milestone: String,
}

async fn set_milestone(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<MilestoneF>) -> Res {
    let ctx = triage_ctx(&app, u.clone(), &owner, &name)?;
    let issue = load_issue(&app, &ctx, number)?;
    let m = f.milestone.parse::<i64>().ok();
    if app.db.set_issue_milestone(ctx.repo.id, &issue, m, u.id).is_err() {
        return render_issue(&app, &ctx, number, None, Some("No such milestone."));
    }
    Ok(back(&ctx, &issue))
}

#[derive(Deserialize)]
struct BodyF {
    body: String,
}

async fn comment_edit(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>, Form(f): Form<BodyF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let c = app.db.comment(id)?.ok_or(Err::NotFound)?;
    let issue = issue_of_comment(&app, &ctx, c.issue_id)?;
    if ctx.repo.archived || c.author_id != Some(u.id) {
        return render_issue(&app, &ctx, issue.number, None, Some("You can only edit your own comments."));
    }
    if f.body.trim().is_empty() {
        return render_issue(&app, &ctx, issue.number, None, Some("The comment is empty."));
    }
    if let Err(e) = check_text(None, &f.body) {
        return render_issue(&app, &ctx, issue.number, None, Some(e));
    }
    app.db.edit_comment(id, &f.body)?;
    record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
    Ok(see_other(&format!("{}/{}/{}#c{id}", ctx.base(), kind_of(&issue), issue.number)))
}

async fn comment_delete(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let c = app.db.comment(id)?.ok_or(Err::NotFound)?;
    let issue = issue_of_comment(&app, &ctx, c.issue_id)?;
    if c.author_id != Some(u.id) && ctx.access < Access::Admin {
        return render_issue(&app, &ctx, issue.number, None, Some("You can't delete this comment."));
    }
    app.db.delete_comment(id, issue.id)?;
    Ok(back(&ctx, &issue))
}

/// The comment's issue, which must belong to this repo (no cross-repo id games).
fn issue_of_comment(app: &App, ctx: &Ctx, issue_id: i64) -> Result<Issue, Err> {
    let number: i64 = app
        .db
        .c()
        .query_row("SELECT number FROM issues WHERE id = ?1 AND repo_id = ?2", rusqlite::params![issue_id, ctx.repo.id], |r| r.get(0))
        .map_err(|_| Err::NotFound)?;
    app.db.issue(ctx.repo.id, number)?.ok_or(Err::NotFound)
}

// ---------- labels ----------

fn labels_response(app: &App, ctx: &Ctx, msg: Option<&str>, err: Option<&str>) -> Res {
    let labels = app.db.labels(ctx.repo.id)?;
    let counts = app.db.label_open_counts(ctx.repo.id)?;
    let editable = can_triage(ctx) && !ctx.repo.archived;
    let body = html! { (ctx.header(ui::Tab::Issues, "HEAD")) (uii::labels_page(&ctx.base(), &labels, &counts, editable, msg, err)) };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page("labels", body)).into_response())
}

async fn labels(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    labels_response(&app, &ctx, None, None)
}

#[derive(Deserialize)]
struct LabelF {
    name: String,
    color: String,
    description: String,
}

fn check_label(f: &LabelF) -> Result<(), &'static str> {
    if f.name.trim().is_empty() || f.name.chars().count() > 50 {
        return Err("Label names must be 1–50 characters.");
    }
    if !issues_db::valid_color(&f.color) {
        return Err("Color must look like #a1b2c3.");
    }
    if f.description.chars().count() > 200 {
        return Err("Description is too long.");
    }
    Ok(())
}

pub fn is_unique_violation(e: &crate::db::Error) -> bool {
    matches!(e, crate::db::Error::SqliteFailure(x, _) if x.code == rusqlite::ErrorCode::ConstraintViolation)
}

async fn label_create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<LabelF>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    if let Err(e) = check_label(&f) {
        return labels_response(&app, &ctx, None, Some(e));
    }
    match app.db.create_label(ctx.repo.id, f.name.trim(), &f.color, f.description.trim()) {
        Ok(()) => labels_response(&app, &ctx, Some("Label created."), None),
        Err(e) if is_unique_violation(&e) => labels_response(&app, &ctx, None, Some("A label with that name exists.")),
        Err(e) => Err(e.into()),
    }
}

async fn label_update(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>, Form(f): Form<LabelF>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    if let Err(e) = check_label(&f) {
        return labels_response(&app, &ctx, None, Some(e));
    }
    match app.db.update_label(ctx.repo.id, id, f.name.trim(), &f.color, f.description.trim()) {
        Ok(()) => labels_response(&app, &ctx, Some("Label saved."), None),
        Err(e) if is_unique_violation(&e) => labels_response(&app, &ctx, None, Some("A label with that name exists.")),
        Err(e) => Err(e.into()),
    }
}

async fn label_delete(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    app.db.delete_label(ctx.repo.id, id)?;
    labels_response(&app, &ctx, Some("Label deleted."), None)
}

// ---------- milestones ----------

fn milestones_response(app: &App, ctx: &Ctx, msg: Option<&str>, err: Option<&str>) -> Res {
    let ms = app.db.milestones(ctx.repo.id)?;
    let editable = can_triage(ctx) && !ctx.repo.archived;
    let body = html! { (ctx.header(ui::Tab::Issues, "HEAD")) (uii::milestones_page(&ctx.base(), &ms, editable, msg, err)) };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page("milestones", body)).into_response())
}

async fn milestones(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    milestones_response(&app, &ctx, None, None)
}

#[derive(Deserialize)]
struct MilestoneEditF {
    title: String,
    description: String,
    due: Option<String>,
    closed: Option<String>,
}

fn check_milestone(f: &MilestoneEditF) -> Result<Option<i64>, &'static str> {
    if f.title.trim().is_empty() || f.title.chars().count() > 100 {
        return Err("Milestone titles must be 1–100 characters.");
    }
    if f.description.chars().count() > 500 {
        return Err("Description is too long.");
    }
    match f.due.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(d) => uii::parse_date(d).map(Some).ok_or("Due date must be YYYY-MM-DD."),
    }
}

async fn milestone_create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<MilestoneEditF>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    let due = match check_milestone(&f) {
        Ok(d) => d,
        Err(e) => return milestones_response(&app, &ctx, None, Some(e)),
    };
    match app.db.create_milestone(ctx.repo.id, f.title.trim(), f.description.trim(), due) {
        Ok(()) => milestones_response(&app, &ctx, Some("Milestone created."), None),
        Err(e) if is_unique_violation(&e) => milestones_response(&app, &ctx, None, Some("A milestone with that title exists.")),
        Err(e) => Err(e.into()),
    }
}

async fn milestone_update(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>, Form(f): Form<MilestoneEditF>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    let due = match check_milestone(&f) {
        Ok(d) => d,
        Err(e) => return milestones_response(&app, &ctx, None, Some(e)),
    };
    match app.db.update_milestone(ctx.repo.id, id, f.title.trim(), f.description.trim(), due, f.closed.is_some()) {
        Ok(()) => milestones_response(&app, &ctx, Some("Milestone saved."), None),
        Err(e) if is_unique_violation(&e) => milestones_response(&app, &ctx, None, Some("A milestone with that title exists.")),
        Err(e) => Err(e.into()),
    }
}

async fn milestone_delete(State(app): St, Authed(u): Authed, Path((owner, name, id)): Path<(String, String, i64)>) -> Res {
    let ctx = triage_ctx(&app, u, &owner, &name)?;
    app.db.delete_milestone(ctx.repo.id, id)?;
    milestones_response(&app, &ctx, Some("Milestone deleted."), None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn form_pairs_repeated_and_encoded() {
        let p = super::form_pairs(b"label=1&label=22&name=a+b%2Fc&bad=%zz&end=%4");
        assert_eq!(p[0], ("label".into(), "1".into()));
        assert_eq!(p[1], ("label".into(), "22".into()));
        assert_eq!(p[2], ("name".into(), "a b/c".into()));
        assert_eq!(p[3], ("bad".into(), "%zz".into()));
        assert_eq!(p[4], ("end".into(), "%4".into()));
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/issues", get(list))
        .route("/{owner}/{name}/issues/new", get(new_page).post(create))
        .route("/{owner}/{name}/issues/{number}", get(show))
        .route("/{owner}/{name}/issues/{number}/comments", post(comment))
        .route("/{owner}/{name}/issues/{number}/edit", post(edit))
        .route("/{owner}/{name}/issues/{number}/labels", post(set_labels))
        .route("/{owner}/{name}/issues/{number}/assignees", post(assign))
        .route("/{owner}/{name}/issues/{number}/milestone", post(set_milestone))
        .route("/{owner}/{name}/pulls", get(list_pulls))
        .route("/{owner}/{name}/pulls/{number}/comments", post(comment))
        .route("/{owner}/{name}/pulls/{number}/edit", post(edit))
        .route("/{owner}/{name}/pulls/{number}/labels", post(set_labels))
        .route("/{owner}/{name}/pulls/{number}/assignees", post(assign))
        .route("/{owner}/{name}/pulls/{number}/milestone", post(set_milestone))
        .route("/{owner}/{name}/comments/{id}/edit", post(comment_edit))
        .route("/{owner}/{name}/comments/{id}/delete", post(comment_delete))
        .route("/{owner}/{name}/labels", get(labels).post(label_create))
        .route("/{owner}/{name}/labels/{id}", post(label_update))
        .route("/{owner}/{name}/labels/{id}/delete", post(label_delete))
        .route("/{owner}/{name}/milestones", get(milestones).post(milestone_create))
        .route("/{owner}/{name}/milestones/{id}", post(milestone_update))
        .route("/{owner}/{name}/milestones/{id}/delete", post(milestone_delete))
}
