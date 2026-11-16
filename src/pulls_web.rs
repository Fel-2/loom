//! Pull request routes: compare, create, conversation, commits, files with
//! inline comments, revisions, reviews, merge.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;

use crate::app::App;
use crate::db::{Access, Repo, User};
use crate::gitops::{self, Ident, MergeCheck};
use crate::hooks;
use crate::issues_db::Issue;
use crate::issues_web::{self, Refs};
use crate::pulls_db::Pull;
use crate::ui::{self, Tab};
use crate::ui_issues::Item;
use crate::ui_pulls::{self as uip, Mergeable, PrTab};
use crate::web::{blocking, see_other, Authed, Ctx, Err, Res, St, Viewer};

const MAX_COMMITS: usize = 250;

/// Where the PR's commits come from.
struct Head {
    repo: Option<Repo>,
    label: String,
}

fn head_info(app: &App, ctx: &Ctx, pull: &Pull) -> Head {
    let repo = pull.head_repo_id.and_then(|id| app.db.repo_by_id(id).ok().flatten());
    let label = match &repo {
        Some(r) if r.id == ctx.repo.id => pull.head_branch.clone(),
        Some(r) => format!("{}/{}:{}", r.owner, r.name, pull.head_branch),
        None => format!("(deleted fork):{}", pull.head_branch),
    };
    Head { repo, label }
}

fn load_pull(app: &App, ctx: &Ctx, number: i64) -> Result<(Issue, Pull), Err> {
    let issue = app.db.issue(ctx.repo.id, number)?.filter(|i| i.is_pr).ok_or(Err::NotFound)?;
    let pull = app.db.pull(ctx.repo.id, number)?.ok_or(Err::NotFound)?;
    Ok((issue, pull))
}

/// (merge-base, head) for the PR's diff. Merged PRs use the base recorded at
/// their last revision (the live base already contains the head).
fn diff_range(app: &App, dir: &std::path::Path, issue_id: i64, pull: &Pull) -> Option<(String, String)> {
    let head = pull.head_sha.clone();
    if pull.merged {
        let base = app.db.revisions(issue_id).ok()?.last()?.base_sha.clone();
        return gitops::is_sha(&base).then_some((base, head));
    }
    let tip = gitops::branch_sha(dir, &pull.base_branch)?;
    Some((gitops::merge_base(dir, &tip, &head)?, head))
}

struct Diff {
    commits: Vec<crate::git::CommitInfo>,
    files: Vec<crate::git::FileDiff>,
}

fn pr_diff(app: &App, ctx: &Ctx, issue_id: i64, pull: &Pull, with_files: bool) -> Diff {
    let Some((base, head)) = diff_range(app, &ctx.dir, issue_id, pull) else {
        return Diff { commits: Vec::new(), files: Vec::new() };
    };
    let Ok(repo) = crate::git::open(&ctx.dir) else { return Diff { commits: Vec::new(), files: Vec::new() } };
    let commits = gitops::rev_list(&ctx.dir, &base, &head, MAX_COMMITS)
        .iter()
        .filter_map(|s| crate::git::parse_id(s))
        .filter_map(|id| crate::git::commit(&repo, id).ok())
        .rev()
        .collect();
    let files = match (with_files, crate::git::parse_id(&base), crate::git::parse_id(&head)) {
        (true, Some(b), Some(h)) => crate::git::diff_commits(&repo, Some(b), h).unwrap_or_default(),
        _ => Vec::new(),
    };
    Diff { commits, files }
}

fn files_count(app: &App, ctx: &Ctx, issue_id: i64, pull: &Pull) -> usize {
    pr_diff(app, ctx, issue_id, pull, true).files.len()
}

fn head_markup(app: &App, ctx: &Ctx, issue: &Issue, pull: &Pull, tab: PrTab, diff: &Diff) -> maud::Markup {
    let h = head_info(app, ctx, pull);
    let revisions = app.db.revisions(issue.id).map(|r| r.len()).unwrap_or(0);
    let files = if diff.files.is_empty() && !diff.commits.is_empty() { files_count(app, ctx, issue.id, pull) } else { diff.files.len() };
    let base = ctx.base();
    uip::head(&uip::HeadInfo { base: &base, issue, pull, head_label: &h.label, commits: diff.commits.len(), files, revisions }, tab)
}

/// Required status contexts on the base branch that aren't green on the head.
fn failing_checks(app: &App, repo: &Repo, pull: &Pull) -> (Vec<crate::social_db::Status>, Vec<String>, Vec<String>) {
    let statuses = app.db.statuses(repo.id, &pull.head_sha).unwrap_or_default();
    let required: Vec<String> = app
        .db
        .protections(repo.id)
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.branch == pull.base_branch)
        .map(|p| p.required_checks)
        .unwrap_or_default();
    let failing = required
        .iter()
        .filter(|c| !statuses.iter().any(|s| &s.context == *c && s.state == "success"))
        .cloned()
        .collect();
    (statuses, required, failing)
}

fn mergeable(dir: &std::path::Path, pull: &Pull) -> Mergeable {
    let Some(tip) = gitops::branch_sha(dir, &pull.base_branch) else {
        return Mergeable::Unknown(format!("base branch '{}' no longer exists", pull.base_branch));
    };
    if gitops::is_ancestor(dir, &pull.head_sha, &tip) {
        return Mergeable::NothingToMerge;
    }
    if gitops::is_ancestor(dir, &tip, &pull.head_sha) {
        return Mergeable::FastForward;
    }
    match gitops::merge_check(dir, &tip, &pull.head_sha) {
        Ok(MergeCheck::Clean { .. }) => Mergeable::Clean,
        Ok(MergeCheck::Conflicts(f)) => Mergeable::Conflicts(f),
        Err(e) => Mergeable::Unknown(e),
    }
}

/// Latest verdict per reviewer (a later "comment" doesn't erase a verdict).
fn latest_reviews(reviews: &[crate::pulls_db::Review]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    for r in reviews {
        let Some(a) = &r.author else { continue };
        match v.iter_mut().find(|(n, _)| n == a) {
            Some(e) if r.state != "comment" => e.1 = r.state.clone(),
            Some(_) => {}
            None => v.push((a.clone(), r.state.clone())),
        }
    }
    v
}

// ---------- conversation ----------

pub fn render_pull(app: &Arc<App>, ctx: &Ctx, issue: Issue, msg: Option<&str>, err: Option<&str>) -> Res {
    let pull = app.db.pull(ctx.repo.id, issue.number)?.ok_or(Err::NotFound)?;
    let viewer = ctx.viewer.as_ref();
    if let Some(u) = viewer {
        let _ = app.db.mark_read(u.id, issue.id);
    }
    let mut parts = issues_web::issue_parts(app, ctx, &issue)?;
    let refs = Refs { app, viewer, repo: &ctx.repo, git: crate::git::open(&ctx.dir).ok() };
    let pbase = format!("{}/pulls/{}", ctx.base(), issue.number);

    let reviews = app.db.reviews(issue.id)?;
    for r in &reviews {
        let html = if r.body.is_empty() { maud::PreEscaped(String::new()) } else { ui::markdown_refs(&r.body, &refs) };
        parts.items.push((r.created, Item::Review { author: r.author.clone(), state: r.state.clone(), html, created: r.created }));
    }
    for c in app.db.review_comments(issue.id)? {
        let outdated = c.head_sha != pull.head_sha;
        let link = format!("{pbase}/files#rc{}", c.id);
        parts.items.push((
            c.created,
            Item::Code { author: c.author.clone(), path: c.path.clone(), line: c.line, html: ui::markdown_refs(&c.body, &refs), created: c.created, outdated, link },
        ));
    }

    let diff = pr_diff(app, ctx, issue.id, &pull, false);
    let head = head_info(app, ctx, &pull);
    let latest = latest_reviews(&reviews);
    let triage = ctx.access >= Access::Write;
    let head_exists = head.repo.as_ref().is_some_and(|r| gitops::branch_sha(&app.repo_dir(r), &pull.head_branch).as_deref() == Some(pull.head_sha.as_str()));
    let can_delete_branch = head.repo.as_ref().is_some_and(|r| {
        app.db.access(viewer, r).is_ok_and(|a| a >= Access::Write) && !(r.id == ctx.repo.id && pull.head_branch == pull.base_branch)
    });
    let state = if issue.closed { Mergeable::Unknown(String::new()) } else { mergeable(&ctx.dir, &pull) };
    let (statuses, required, failing) = failing_checks(app, &ctx.repo, &pull);
    let blocked = if !failing.is_empty() {
        Some(format!("Required checks must pass first: {}.", failing.join(", ")))
    } else if !triage && !issue.closed {
        Some("Only people with write access can merge.".into())
    } else {
        None
    };
    let mb = uip::MergeBox {
        pbase: &pbase,
        pull: &pull,
        state,
        can_merge: triage && !ctx.repo.archived && failing.is_empty(),
        blocked_reason: blocked,
        checks: crate::ui_social::statuses(&statuses, &required),
        can_delete_branch,
        head_exists,
        approvals: latest.iter().filter(|(_, s)| s == "approve").count(),
        changes_requested: latest.iter().filter(|(_, s)| s == "changes").count(),
    };
    let is_author = viewer.is_some_and(|u| Some(u.id) == issue.author_id);
    let review_form = (viewer.is_some() && !issue.closed && !ctx.repo.archived).then(|| uip::review_form(&pbase, is_author));

    let head_html = head_markup(app, ctx, &issue, &pull, PrTab::Conversation, &diff);
    let base = ctx.base();
    let mut d = issues_web::show_data(ctx, &base, &issue, parts, msg, err);
    d.head = Some(head_html);
    d.merge_box = Some(html! { (uip::merge_box(&mb)) @if let Some(f) = &review_form { (f) } });
    d.extra_side = Some(uip::reviewers(&latest));
    let body = html! { (ctx.header(Tab::Pulls, "HEAD")) (crate::ui_issues::show(&d)) };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page(&format!("{} #{}", issue.title, issue.number), body)).into_response())
}

async fn show(State(app): St, Viewer(v): Viewer, Path((owner, name, number)): Path<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let (issue, _) = load_pull(&app, &ctx, number)?;
    blocking(move || render_pull(&app, &ctx, issue, None, None)).await
}

async fn commits(State(app): St, Viewer(v): Viewer, Path((owner, name, number)): Path<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        let diff = pr_diff(&app, &ctx, issue.id, &pull, false);
        let head = head_markup(&app, &ctx, &issue, &pull, PrTab::Commits, &diff);
        let base = ctx.base();
        let body = html! {
            (ctx.header(Tab::Pulls, "HEAD")) (head)
            div.card.flush { (ui::commit_rows(&base, &diff.commits)) }
            @if diff.commits.len() >= MAX_COMMITS { p.muted.small { "Showing the first " (MAX_COMMITS) " commits." } }
        };
        Ok(ctx.page(&format!("commits · #{number}"), body))
    })
    .await
}

#[derive(Deserialize)]
struct FilesQ {
    at: Option<String>,
}

fn render_files(app: &Arc<App>, ctx: &Ctx, number: i64, at: Option<&str>, err: Option<&str>) -> Res {
    let (issue, pull) = load_pull(app, ctx, number)?;
    let viewer = ctx.viewer.as_ref();
    let diff = pr_diff(app, ctx, issue.id, &pull, true);
    let head = head_markup(app, ctx, &issue, &pull, PrTab::Files, &diff);
    let refs = Refs { app, viewer, repo: &ctx.repo, git: crate::git::open(&ctx.dir).ok() };
    let comments: Vec<uip::InlineComment> = app
        .db
        .review_comments(issue.id)?
        .into_iter()
        .filter(|c| c.head_sha == pull.head_sha)
        .map(|c| uip::InlineComment {
            id: c.id,
            can_delete: viewer.is_some_and(|u| Some(u.id) == c.author_id) || ctx.access >= Access::Admin,
            html: ui::markdown_refs(&c.body, &refs),
            author: c.author,
            created: c.created,
            path: c.path,
            side: c.side,
            line: c.line,
        })
        .collect();
    let at = at.and_then(|a| {
        let (rest, line) = a.rsplit_once(':')?;
        let (path, side) = rest.rsplit_once(':')?;
        Some((path, side, line.parse::<i64>().ok()?))
    });
    let pbase = format!("{}/pulls/{number}/files", ctx.base());
    let can_comment = viewer.is_some() && !ctx.repo.archived;
    let base = ctx.base();
    let fv = uip::FilesView {
        base: &base,
        files: &diff.files,
        comments: &comments,
        at,
        form_action: can_comment.then_some(pbase.as_str()),
        blob_rev: Some(&pull.head_sha),
    };
    let body = html! { (ctx.header(Tab::Pulls, "HEAD")) (head) (ui::flash_box(err, None)) (uip::files_view(&fv)) };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page(&format!("files · #{number}"), body)).into_response())
}

async fn files(State(app): St, Viewer(v): Viewer, Path((owner, name, number)): Path<(String, String, i64)>, Query(q): Query<FilesQ>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || render_files(&app, &ctx, number, q.at.as_deref(), None)).await
}

#[derive(Deserialize)]
struct RevQ {
    from: Option<i64>,
    to: Option<i64>,
}

async fn revisions(State(app): St, Viewer(v): Viewer, Path((owner, name, number)): Path<(String, String, i64)>, Query(q): Query<RevQ>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        let revs = app.db.revisions(issue.id)?;
        let last = revs.last().map_or(1, |r| r.n);
        let to = q.to.unwrap_or(last);
        let from = q.from.unwrap_or((to - 1).max(1));
        let (mut rd, mut err) = (None, None);
        if revs.len() > 1 {
            let a = revs.iter().find(|r| r.n == from);
            let b = revs.iter().find(|r| r.n == to);
            match (a, b) {
                (Some(a), Some(b)) if gitops::is_sha(&a.base_sha) && gitops::is_sha(&b.base_sha) => {
                    match gitops::range_diff(&ctx.dir, &a.base_sha, &a.head_sha, &b.base_sha, &b.head_sha) {
                        Ok(t) => rd = Some(t),
                        Err(e) => err = Some(format!("range-diff failed: {e}")),
                    }
                }
                _ => err = Some("Unknown revision.".to_owned()),
            }
        }
        let diff = pr_diff(&app, &ctx, issue.id, &pull, false);
        let head = head_markup(&app, &ctx, &issue, &pull, PrTab::Revisions, &diff);
        let pbase = format!("{}/pulls/{number}", ctx.base());
        let body = html! { (ctx.header(Tab::Pulls, "HEAD")) (head) (uip::revisions(&pbase, &revs, from, to, rd.as_deref(), err.as_deref())) };
        Ok(ctx.page(&format!("revisions · #{number}"), body))
    })
    .await
}

// ---------- compare & create ----------

#[derive(Deserialize, Default)]
struct CompareQ {
    base: Option<String>,
    head: Option<String>,
}

/// Resolve a head spec: "branch" (this repo) or "owner/repo:branch" (a fork).
fn resolve_head(app: &App, viewer: Option<&User>, base_repo: &Repo, spec: &str) -> Option<(Repo, String)> {
    let (repo, branch) = match spec.split_once(':') {
        Some((full, branch)) => {
            let (o, n) = full.split_once('/')?;
            let (r, _) = app.authorize(viewer, o, n, Access::Read)?;
            if r.id != base_repo.id && r.fork_of != Some(base_repo.id) {
                return None;
            }
            (r, branch.to_owned())
        }
        None => (base_repo.clone(), spec.to_owned()),
    };
    gitops::valid_branch(&branch).then_some((repo, branch))
}

fn compare_page(app: &Arc<App>, ctx: &Ctx, q: &CompareQ, title: &str, body_text: &str, err: Option<&str>) -> Res {
    let viewer = ctx.viewer.as_ref();
    let branches = crate::git::branch_names(&ctx.dir);
    let default = crate::git::open(&ctx.dir).ok().and_then(|r| crate::git::default_branch(&r)).unwrap_or_else(|| "main".into());
    let mut heads: Vec<(String, String)> = branches.iter().map(|b| (b.clone(), b.clone())).collect();
    for f in app.db.forks_of(ctx.repo.id)? {
        if app.db.access(viewer, &f)? < Access::Read {
            continue;
        }
        for b in crate::git::branch_names(&app.repo_dir(&f)) {
            let v = format!("{}/{}:{b}", f.owner, f.name);
            heads.push((v.clone(), v));
        }
    }
    let sel_base = q.base.clone().unwrap_or(default);
    let sel_head = q.head.clone().unwrap_or_default();
    let base = ctx.base();
    let form = uip::compare_form(&uip::CompareForm { base: &base, branches: &branches, heads: &heads, sel_base: &sel_base, sel_head: &sel_head });

    let result = if sel_head.is_empty() {
        html! { p.muted { "Pick a branch (or a fork's branch) to compare against " code { (sel_base) } "." } }
    } else {
        match compare_data(app, ctx, &sel_base, &sel_head) {
            Ok((commits, files, existing)) => {
                let default_title = commits.first().map(|c| c.summary.clone()).unwrap_or_default();
                let t = if title.is_empty() && commits.len() == 1 { default_title.as_str() } else { title };
                uip::compare_result(&uip::CompareResult {
                    base: &base,
                    commits: &commits,
                    files: &files,
                    sel_base: &sel_base,
                    sel_head: &sel_head,
                    existing,
                    can_create: viewer.is_some() && !ctx.repo.archived,
                    title: t,
                    body: body_text,
                    err,
                })
            }
            Err(e) => html! { (ui::flash_box(Some(&e), None)) },
        }
    };
    let body = html! { (ctx.header(Tab::Pulls, "HEAD")) div.stack { (form) (result) } };
    let status = if err.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, ctx.page("compare", body)).into_response())
}

type CompareData = (Vec<crate::git::CommitInfo>, Vec<crate::git::FileDiff>, Option<i64>);

/// Commits/files of head vs base. Fork heads are fetched into a scratch ref
/// so the diff can be computed inside the base repo.
fn compare_data(app: &App, ctx: &Ctx, base_branch: &str, head_spec: &str) -> Result<CompareData, String> {
    let viewer = ctx.viewer.as_ref();
    if !gitops::valid_branch(base_branch) {
        return Err("Not a valid base branch.".into());
    }
    let base_tip = gitops::branch_sha(&ctx.dir, base_branch).ok_or(format!("No branch '{base_branch}' here."))?;
    let (head_repo, head_branch) = resolve_head(app, viewer, &ctx.repo, head_spec).ok_or("Unknown head. Use a branch of this repo or of one of its forks.")?;
    let head_sha = stage_head(app, ctx, &head_repo, &head_branch)?;
    let Some(mb) = gitops::merge_base(&ctx.dir, &base_tip, &head_sha) else {
        unstage(ctx, &head_repo, &head_branch);
        return Err("These branches share no history.".into());
    };
    let repo = crate::git::open(&ctx.dir).map_err(|_| "cannot open repository")?;
    let commits = gitops::rev_list(&ctx.dir, &mb, &head_sha, MAX_COMMITS)
        .iter()
        .filter_map(|s| crate::git::parse_id(s))
        .filter_map(|id| crate::git::commit(&repo, id).ok())
        .rev()
        .collect();
    let files = match (crate::git::parse_id(&mb), crate::git::parse_id(&head_sha)) {
        (Some(b), Some(h)) => crate::git::diff_commits(&repo, Some(b), h).unwrap_or_default(),
        _ => Vec::new(),
    };
    unstage(ctx, &head_repo, &head_branch);
    let existing = app.db.open_pull_for(ctx.repo.id, head_repo.id, &head_branch, base_branch).map_err(|e| e.to_string())?;
    Ok((commits, files, existing))
}

/// Head commit available in the base repo: same repo -> branch tip; fork ->
/// fetched into `refs/loom/compare/<fork-id>/<branch>`.
fn stage_head(app: &App, ctx: &Ctx, head_repo: &Repo, branch: &str) -> Result<String, String> {
    if head_repo.id == ctx.repo.id {
        return gitops::branch_sha(&ctx.dir, branch).ok_or(format!("No branch '{branch}' here."));
    }
    let src: PathBuf = app.repo_dir(head_repo);
    gitops::branch_sha(&src, branch).ok_or(format!("No branch '{branch}' in {}/{}.", head_repo.owner, head_repo.name))?;
    let scratch = scratch_ref(head_repo, branch);
    gitops::fetch_branch(&ctx.dir, &src, branch, &scratch)?;
    gitops::rev_parse(&ctx.dir, &scratch).ok_or_else(|| "fetch failed".into())
}

fn scratch_ref(head_repo: &Repo, branch: &str) -> String {
    format!("refs/loom/compare/{}/{branch}", head_repo.id)
}

/// Drop the scratch ref: a fork's commits must not stay reachable (and
/// fetchable) in the base repo just because someone looked at a compare.
fn unstage(ctx: &Ctx, head_repo: &Repo, branch: &str) {
    if head_repo.id != ctx.repo.id {
        gitops::drop_ref(&ctx.dir, &scratch_ref(head_repo, branch));
    }
}

async fn compare(State(app): St, Viewer(v): Viewer, Path((owner, name)): Path<(String, String)>, Query(q): Query<CompareQ>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    blocking(move || compare_page(&app, &ctx, &q, "", "", None)).await
}

#[derive(Deserialize)]
struct NewPrF {
    base: String,
    head: String,
    title: String,
    body: String,
}

async fn create(State(app): St, Authed(u): Authed, Path((owner, name)): Path<(String, String)>, Form(f): Form<NewPrF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    if ctx.repo.archived {
        return Err(Err::NotFound);
    }
    blocking(move || {
        let q = CompareQ { base: Some(f.base.clone()), head: Some(f.head.clone()) };
        let fail = |e: &str| compare_page(&app, &ctx, &q, &f.title, &f.body, Some(e));
        if f.title.trim().is_empty() || f.title.chars().count() > 256 {
            return fail("A title (max 256 characters) is required.");
        }
        if f.body.len() > 65536 {
            return fail("The description is too long.");
        }
        let Some((head_repo, head_branch)) = resolve_head(&app, Some(&u), &ctx.repo, &f.head) else {
            return fail("Unknown head branch.");
        };
        if head_repo.id == ctx.repo.id && head_branch == f.base {
            return fail("Head and base are the same branch.");
        }
        let Some(base_tip) = gitops::branch_sha(&ctx.dir, &f.base) else { return fail("Unknown base branch.") };
        let head_sha = match stage_head(&app, &ctx, &head_repo, &head_branch) {
            Ok(s) => s,
            Err(e) => return fail(&e),
        };
        let mb = gitops::merge_base(&ctx.dir, &base_tip, &head_sha);
        let ahead = mb.as_ref().is_some_and(|mb| !gitops::rev_list(&ctx.dir, mb, &head_sha, 1).is_empty());
        let existing = app.db.open_pull_for(ctx.repo.id, head_repo.id, &head_branch, &f.base)?;
        if existing.is_some() || mb.is_none() || !ahead {
            unstage(&ctx, &head_repo, &head_branch);
        }
        if let Some(n) = existing {
            return Ok(see_other(&format!("{}/pulls/{n}", ctx.base())));
        }
        let Some(mb) = mb else { return fail("These branches share no history.") };
        if !ahead {
            return fail("Nothing to merge: the base already has these commits.");
        }
        let (issue_id, number) = app.db.create_pull(ctx.repo.id, u.id, f.title.trim(), &f.body, head_repo.id, &head_branch, &f.base, &head_sha, &mb)?;
        if let Err(e) = gitops::force_ref(&ctx.dir, &format!("refs/pull/{number}/head"), &head_sha) {
            eprintln!("pin PR #{number}: {e}");
        }
        unstage(&ctx, &head_repo, &head_branch);
        if let Some(issue) = app.db.issue(ctx.repo.id, number)? {
            issues_web::record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
            if let Some(p) = app.db.pull(ctx.repo.id, number)? {
                crate::events::emit(&app.bus(), crate::events::Ev::Pull { repo: &ctx.repo, actor: Some(&u), issue: &issue, pull: &p, action: "opened" });
            }
        }
        let _ = issue_id;
        Ok(see_other(&format!("{}/pulls/{number}", ctx.base())))
    })
    .await
}

// ---------- review & inline comments ----------

#[derive(Deserialize)]
struct ReviewF {
    state: String,
    body: String,
}

async fn review(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<ReviewF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        let err = if ctx.repo.archived || issue.closed {
            Some("This pull request is closed.")
        } else if !["comment", "approve", "changes"].contains(&f.state.as_str()) {
            Some("Unknown review state.")
        } else if f.state != "comment" && Some(u.id) == issue.author_id {
            Some("You can't approve or request changes on your own pull request.")
        } else if f.state != "approve" && f.body.trim().is_empty() {
            Some("Say something: comments and change requests need text.")
        } else if f.body.len() > 65536 {
            Some("The review is too long.")
        } else {
            None
        };
        if let Some(e) = err {
            return render_pull(&app, &ctx, issue, None, Some(e));
        }
        app.db.add_review(issue.id, u.id, &f.state, f.body.trim(), &pull.head_sha)?;
        issues_web::record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
        let state = match f.state.as_str() { "approve" => "approved", "changes" => "changes_requested", _ => "commented" };
        crate::events::emit(&app.bus(), crate::events::Ev::Review { repo: &ctx.repo, actor: &u, issue: &issue, pull: &pull, state, body: f.body.trim() });
        Ok(see_other(&format!("{}/pulls/{number}", ctx.base())))
    })
    .await
}

#[derive(Deserialize)]
struct InlineF {
    path: String,
    side: String,
    line: i64,
    body: String,
}

async fn inline_comment(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<InlineF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        let at = format!("{}:{}:{}", f.path, f.side, f.line);
        let bad = if ctx.repo.archived {
            Some("This repository is archived.")
        } else if !(f.side == "L" || f.side == "R") || f.line <= 0 || f.path.is_empty() || f.path.len() > 4096 {
            Some("Bad comment position.")
        } else if f.body.trim().is_empty() || f.body.len() > 65536 {
            Some("The comment is empty or too long.")
        } else {
            None
        };
        if let Some(e) = bad {
            return render_files(&app, &ctx, number, Some(&at), Some(e));
        }
        // The line must exist in the current diff.
        let diff = pr_diff(&app, &ctx, issue.id, &pull, true);
        let exists = diff.files.iter().filter(|fd| fd.path == f.path).flat_map(|fd| fd.lines.iter()).any(|l| match l {
            crate::git::Line::Add(b, _) | crate::git::Line::Ctx(_, b, _) => f.side == "R" && *b as i64 == f.line,
            crate::git::Line::Del(a, _) => f.side == "L" && *a as i64 == f.line,
            crate::git::Line::Hunk(_) => false,
        });
        if !exists {
            return render_files(&app, &ctx, number, None, Some("That line isn't part of this diff."));
        }
        let id = app.db.add_review_comment(issue.id, u.id, &f.path, &f.side, f.line, &pull.head_sha, f.body.trim())?;
        issues_web::record_mentions(&app, &u, &ctx.repo, &issue, &f.body);
        Ok(see_other(&format!("{}/pulls/{number}/files#rc{id}", ctx.base())))
    })
    .await
}

async fn inline_delete(State(app): St, Authed(u): Authed, Path((owner, name, number, id)): Path<(String, String, i64, i64)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    let (issue, _) = load_pull(&app, &ctx, number)?;
    let author = app.db.review_comment_author(issue.id, id)?.ok_or(Err::NotFound)?;
    if author != Some(u.id) && ctx.access < Access::Admin {
        return blocking(move || render_files(&app, &ctx, number, None, Some("You can't delete this comment."))).await;
    }
    app.db.delete_review_comment(issue.id, id)?;
    Ok(see_other(&format!("{}/pulls/{number}/files", ctx.base())))
}

// ---------- merge ----------

#[derive(Deserialize)]
struct MergeF {
    strategy: String,
    expected_head: String,
}

async fn merge(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>, Form(f): Form<MergeF>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    if ctx.access < Access::Write || ctx.repo.archived {
        return Err(Err::NotFound);
    }
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        match do_merge(&app, &ctx, &u, &issue, &pull, &f) {
            Ok(()) => Ok(see_other(&format!("{}/pulls/{number}", ctx.base()))),
            Err(e) => render_pull(&app, &ctx, issue, None, Some(&e)),
        }
    })
    .await
}

fn do_merge(app: &App, ctx: &Ctx, u: &User, issue: &Issue, pull: &Pull, f: &MergeF) -> Result<(), String> {
    if issue.closed {
        return Err("This pull request is closed.".into());
    }
    if f.expected_head != pull.head_sha {
        return Err("The pull request changed while you were looking at it. Review the new commits, then merge.".into());
    }
    let (_, _, failing) = failing_checks(app, &ctx.repo, pull);
    if !failing.is_empty() {
        return Err(format!("Required checks must pass first: {}.", failing.join(", ")));
    }
    let dir = &ctx.dir;
    let base_ref = format!("refs/heads/{}", pull.base_branch);
    let tip = gitops::branch_sha(dir, &pull.base_branch).ok_or("The base branch no longer exists.")?;
    if gitops::is_ancestor(dir, &pull.head_sha, &tip) {
        return Err("Nothing to merge.".into());
    }
    let me = Ident::of(u);
    let author = issue
        .author_id
        .and_then(|_| issue.author.as_ref())
        .and_then(|n| app.db.user_by_name(n).ok().flatten())
        .map(|a| Ident::of(&a))
        .unwrap_or_else(|| Ident { name: me.name.clone(), email: me.email.clone() });
    let head_label = head_info(app, ctx, pull).label;
    let new = match f.strategy.as_str() {
        "merge" | "squash" => {
            let tree = match gitops::merge_check(dir, &tip, &pull.head_sha)? {
                MergeCheck::Clean { tree } => tree,
                MergeCheck::Conflicts(_) => return Err("There are conflicts; resolve them first.".into()),
            };
            if f.strategy == "merge" {
                let msg = format!("Merge pull request #{} from {head_label}\n\n{}", issue.number, issue.title);
                gitops::commit_tree(dir, &tree, &[&tip, &pull.head_sha], &msg, &me, &me)?
            } else {
                let subjects = gitops::rev_list(dir, &tip, &pull.head_sha, 100)
                    .iter()
                    .filter_map(|s| crate::git::parse_id(s))
                    .filter_map(|id| crate::git::open(dir).ok().and_then(|r| crate::git::commit(&r, id).ok()))
                    .map(|c| format!("* {}", c.summary))
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = format!("{} (#{})\n\n{subjects}", issue.title, issue.number);
                gitops::commit_tree(dir, &tree, &[&tip], &msg, &author, &me)?
            }
        }
        "rebase" => {
            if gitops::is_ancestor(dir, &tip, &pull.head_sha) {
                pull.head_sha.clone()
            } else {
                gitops::replay(dir, &tip, &format!("refs/pull/{}/head", issue.number), &me)?
            }
        }
        _ => return Err("Unknown merge strategy.".into()),
    };
    gitops::update_ref(dir, &base_ref, &new, Some(&tip)).map_err(|_| "The base branch moved during the merge; try again.".to_string())?;
    hooks::merged(&app.db, &app.url, &ctx.repo, pull, Some(u), &new, &f.strategy);
    let up = hooks::Update { old: tip, new, refname: base_ref };
    hooks::after_update(&app.db, &app.data, &app.url, &ctx.repo, Some(u), &[up]);
    Ok(())
}

async fn delete_branch(State(app): St, Authed(u): Authed, Path((owner, name, number)): Path<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, Some(u.clone()), &owner, &name)?;
    blocking(move || {
        let (issue, pull) = load_pull(&app, &ctx, number)?;
        let head = head_info(&app, &ctx, &pull);
        let Some(hr) = head.repo.filter(|r| app.db.access(Some(&u), r).is_ok_and(|a| a >= Access::Write)) else {
            return render_pull(&app, &ctx, issue, None, Some("You can't delete that branch."));
        };
        if !issue.closed {
            return render_pull(&app, &ctx, issue, None, Some("Close or merge the pull request first."));
        }
        let hdir = app.repo_dir(&hr);
        // Only if it still points where the PR left it (no newer work lost).
        if let Err(e) = gitops::delete_ref(&hdir, &format!("refs/heads/{}", pull.head_branch), &pull.head_sha) {
            return render_pull(&app, &ctx, issue, None, Some(&format!("Not deleted: the branch has new commits or is gone ({e}).")));
        }
        let _ = app.db.add_event(issue.id, Some(u.id), "head_deleted", &pull.head_branch, None);
        Ok(see_other(&format!("{}/pulls/{number}", ctx.base())))
    })
    .await
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/compare", get(compare))
        .route("/{owner}/{name}/pulls/new", post(create))
        .route("/{owner}/{name}/pulls/{number}", get(show))
        .route("/{owner}/{name}/pulls/{number}/commits", get(commits))
        .route("/{owner}/{name}/pulls/{number}/files", get(files))
        .route("/{owner}/{name}/pulls/{number}/files/comments", post(inline_comment))
        .route("/{owner}/{name}/pulls/{number}/files/comments/{id}/delete", post(inline_delete))
        .route("/{owner}/{name}/pulls/{number}/revisions", get(revisions))
        .route("/{owner}/{name}/pulls/{number}/review", post(review))
        .route("/{owner}/{name}/pulls/{number}/merge", post(merge))
        .route("/{owner}/{name}/pulls/{number}/delete-branch", post(delete_branch))
}
