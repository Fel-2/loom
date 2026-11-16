//! Views for pull requests: header, merge box, compare, files with inline
//! comments, revisions.

use maud::{html, Markup};

use crate::git::{CommitInfo, FileDiff, Line};
use crate::issues_db::Issue;
use crate::pulls_db::{Pull, Revision};
use crate::ui::{ago, flash_box};
use crate::ui_issues::pr_state_pill;

#[derive(PartialEq, Clone, Copy)]
pub enum PrTab {
    Conversation,
    Commits,
    Files,
    Revisions,
}

pub struct HeadInfo<'a> {
    pub base: &'a str,
    pub issue: &'a Issue,
    pub pull: &'a Pull,
    /// "branch" or "owner/repo:branch"
    pub head_label: &'a str,
    pub commits: usize,
    pub files: usize,
    pub revisions: usize,
}

pub fn head(h: &HeadInfo<'_>, tab: PrTab) -> Markup {
    let i = h.issue;
    let pbase = format!("{}/pulls/{}", h.base, i.number);
    html! {
        div.issue-head {
            h2 { (i.title) " " span.muted { "#" (i.number) } }
            div.meta {
                (pr_state_pill(i.closed, i.merged))
                span.muted {
                    @match &i.author { Some(a) => a.who href={ "/" (a) } { (a) }, None => "ghost" }
                    @if i.merged { " merged " } @else { " wants to merge " }
                    (h.commits) " commit" @if h.commits != 1 { "s" } " into "
                    code.ref-pill { (h.pull.base_branch) } " from " code.ref-pill { (h.head_label) }
                }
            }
            nav.tabs.subtabs {
                a.active[tab == PrTab::Conversation] href=(pbase) { "conversation" @if i.comments > 0 { span.tab-count { (i.comments) } } }
                a.active[tab == PrTab::Commits] href={ (pbase) "/commits" } { "commits" span.tab-count { (h.commits) } }
                a.active[tab == PrTab::Files] href={ (pbase) "/files" } { "files" span.tab-count { (h.files) } }
                a.active[tab == PrTab::Revisions] href={ (pbase) "/revisions" } { "revisions" span.tab-count { (h.revisions) } }
            }
        }
    }
}

#[derive(Clone)]
pub enum Mergeable {
    Clean,
    FastForward,
    Conflicts(Vec<String>),
    NothingToMerge,
    Unknown(String),
}

pub struct MergeBox<'a> {
    pub pbase: &'a str,
    pub pull: &'a Pull,
    pub state: Mergeable,
    pub can_merge: bool,
    pub blocked_reason: Option<String>,
    pub can_delete_branch: bool,
    pub head_exists: bool,
    pub approvals: usize,
    pub changes_requested: usize,
    /// Rendered commit statuses for the head.
    pub checks: Markup,
}

pub fn merge_box(m: &MergeBox<'_>) -> Markup {
    let p = m.pull;
    html! {
        section.card.merge-box {
            @if p.merged {
                div.mb-row.merged {
                    span.big { "⇄" }
                    div {
                        strong { "Merged" }
                        div.muted.small {
                            @if let Some(by) = &p.merged_by { (by) " " }
                            "merged into " code { (p.base_branch) }
                            @if let Some(t) = p.merged_at { " " (ago(t)) }
                            @if let Some(sha) = &p.merge_sha { " as " code { (&sha[..sha.len().min(8)]) } }
                        }
                    }
                }
                @if m.can_delete_branch && m.head_exists {
                    form.inline method="post" action={ (m.pbase) "/delete-branch" } { button.btn { "delete branch " code { (p.head_branch) } } }
                }
            } @else if p.closed {
                div.mb-row { span.big.muted { "✕" } div { strong { "Closed without merging" } } }
            } @else {
                (m.checks)
                @if m.approvals > 0 || m.changes_requested > 0 {
                    div.mb-row.small {
                        @if m.approvals > 0 { span.verdict.ok { "✓ " (m.approvals) " approval" @if m.approvals != 1 { "s" } } }
                        @if m.changes_requested > 0 { " " span.verdict.bad { "✕ " (m.changes_requested) " requesting changes" } }
                    }
                }
                @match &m.state {
                    Mergeable::Clean | Mergeable::FastForward => {
                        div.mb-row {
                            span.big.ok { "✓" }
                            div {
                                strong { "No conflicts with " code { (p.base_branch) } }
                                @if matches!(m.state, Mergeable::FastForward) { div.muted.small { "Can be fast-forwarded." } }
                            }
                        }
                        @if let Some(r) = &m.blocked_reason { p.muted.small { (r) } }
                        @if m.can_merge {
                            form.merge-form method="post" action={ (m.pbase) "/merge" } {
                                label.check { input type="radio" name="strategy" value="merge" checked; "merge commit" }
                                label.check { input type="radio" name="strategy" value="squash"; "squash" }
                                label.check { input type="radio" name="strategy" value="rebase"; "rebase" }
                                input type="hidden" name="expected_head" value=(p.head_sha);
                                button.btn.primary { "merge pull request" }
                            }
                        }
                    }
                    Mergeable::Conflicts(files) => {
                        div.mb-row {
                            span.big.bad { "!" }
                            div {
                                strong { "Conflicts with " code { (p.base_branch) } }
                                div.muted.small { "Resolve locally (merge or rebase " code { (p.base_branch) } ") and push." }
                                @if !files.is_empty() { ul.conflicts { @for f in files { li { code { (f) } } } } }
                            }
                        }
                    }
                    Mergeable::NothingToMerge => {
                        div.mb-row { span.big.muted { "=" } div { strong { "Nothing to merge" } div.muted.small { "The base branch already contains these commits." } } }
                    }
                    Mergeable::Unknown(why) => {
                        div.mb-row { span.big.muted { "?" } div { strong { "Can't check mergeability" } div.muted.small { (why) } } }
                    }
                }
            }
        }
    }
}

pub fn reviewers(latest: &[(String, String)]) -> Markup {
    html! {
        section.card {
            h4 { "reviews" }
            @if latest.is_empty() { p.muted.small { "none yet" } }
            @for (who, state) in latest {
                div.side-row {
                    a href={ "/" (who) } { (who) }
                    @match state.as_str() {
                        "approve" => span.verdict.ok { "✓" },
                        "changes" => span.verdict.bad { "✕" },
                        _ => span.muted { "💬" },
                    }
                }
            }
        }
    }
}

pub fn review_form(pbase: &str, is_author: bool) -> Markup {
    html! {
        details.card.review-form {
            summary { "review" }
            form.form method="post" action={ (pbase) "/review" } {
                textarea.body name="body" rows="5" placeholder="Overall feedback (markdown)" {}
                div.row {
                    label.check { input type="radio" name="state" value="comment" checked; "comment" }
                    @if !is_author {
                        label.check { input type="radio" name="state" value="approve"; "approve" }
                        label.check { input type="radio" name="state" value="changes"; "request changes" }
                    }
                }
                button.btn.primary { "submit review" }
            }
        }
    }
}

// ---------- compare ----------

pub struct CompareForm<'a> {
    pub base: &'a str,
    pub branches: &'a [String],
    /// (value, label) — "branch" or "owner/repo:branch"
    pub heads: &'a [(String, String)],
    pub sel_base: &'a str,
    pub sel_head: &'a str,
}

pub fn compare_form(f: &CompareForm<'_>) -> Markup {
    html! {
        form.card.compare-form method="get" action={ (f.base) "/compare" } {
            span.muted { "base" }
            select name="base" { @for b in f.branches { option value=(b) selected[b == f.sel_base] { (b) } } }
            span.muted { "←" }
            span.muted { "compare" }
            select name="head" { @for (v, l) in f.heads { option value=(v) selected[v == f.sel_head] { (l) } } }
            button.btn { "compare" }
        }
    }
}

pub struct CompareResult<'a> {
    pub base: &'a str,
    pub commits: &'a [CommitInfo],
    pub files: &'a [FileDiff],
    pub sel_base: &'a str,
    pub sel_head: &'a str,
    pub existing: Option<i64>,
    pub can_create: bool,
    pub title: &'a str,
    pub body: &'a str,
    pub err: Option<&'a str>,
}

pub fn compare_result(r: &CompareResult<'_>) -> Markup {
    html! {
        (flash_box(r.err, None))
        @if r.commits.is_empty() {
            div.card { p.muted { "Nothing to compare: " code { (r.sel_head) } " has no commits that " code { (r.sel_base) } " doesn't." } }
        } @else {
            @if let Some(n) = r.existing {
                div.card.notice { "There's already an open pull request for this: " a href={ (r.base) "/pulls/" (n) } { "#" (n) } }
            } @else if r.can_create {
                form.card.form method="post" action={ (r.base) "/pulls/new" } {
                    h3 { "open a pull request" }
                    input type="hidden" name="base" value=(r.sel_base);
                    input type="hidden" name="head" value=(r.sel_head);
                    label { "title" input name="title" value=(r.title) required maxlength="256"; }
                    label { "description" textarea.body name="body" rows="6" placeholder="What does this change? 'Fixes #12' closes the issue on merge." { (r.body) } }
                    button.btn.primary { "create pull request" }
                }
            }
            div.card.flush {
                div.card-head { (r.commits.len()) " commits" }
                (crate::ui::commit_rows(r.base, r.commits))
            }
            (files_view(&FilesView { base: r.base, files: r.files, comments: &[], at: None, form_action: None, blob_rev: None }))
        }
    }
}

// ---------- files with inline comments ----------

pub struct InlineComment {
    pub id: i64,
    pub author: Option<String>,
    pub html: Markup,
    pub created: i64,
    pub can_delete: bool,
    pub path: String,
    pub side: String,
    pub line: i64,
}

pub struct FilesView<'a> {
    pub base: &'a str,
    pub files: &'a [FileDiff],
    /// Current-revision comments to place inline.
    pub comments: &'a [InlineComment],
    /// Open the comment form at (path, side, line).
    pub at: Option<(&'a str, &'a str, i64)>,
    /// `<pr>/files` when commenting is allowed.
    pub form_action: Option<&'a str>,
    pub blob_rev: Option<&'a str>,
}

fn line_key(l: &Line) -> Option<(&'static str, u32)> {
    match l {
        Line::Add(b, _) => Some(("R", *b)),
        Line::Ctx(_, b, _) => Some(("R", *b)),
        Line::Del(a, _) => Some(("L", *a)),
        Line::Hunk(_) => None,
    }
}

pub fn files_view(v: &FilesView<'_>) -> Markup {
    let (adds, dels) = v.files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
    html! {
        p.muted.stat { (v.files.len()) " files changed, " span.add { "+" (adds) } " " span.del { "-" (dels) } }
        @for (i, f) in v.files.iter().enumerate() {
            div.card.flush.diff id={ "f" (i) } {
                div.card-head {
                    span class={ "st st-" (f.status) } { (f.status) }
                    @if let Some(o) = &f.old_path { span.muted { (o) " → " } }
                    @if let Some(rev) = v.blob_rev { a href={ (v.base) "/blob/" (rev) "/" (f.path) } { (f.path) } } @else { span { (f.path) } }
                    span.grow {}
                    span.add { "+" (f.adds) } span.del { "-" (f.dels) }
                }
                @if f.binary {
                    p.pad.muted { "Binary or large file not shown." }
                } @else {
                    table.code {
                        @for l in &f.lines {
                            @let key = line_key(l);
                            @let anchor = key.map(|(s, n)| format!("{}-{}{}", i, s, n));
                            @match l {
                                Line::Hunk(h) => tr.hunk { td.ln {} td.ln {} td.src { (h) } },
                                Line::Ctx(a, b, t) => tr id=[anchor.as_deref()] { td.ln { (a) } td.ln { (cmt_link(v, f, key, i, *b)) } td.src { " " (t) } },
                                Line::Del(a, t) => tr.del id=[anchor.as_deref()] { td.ln { (cmt_link(v, f, key, i, *a)) } td.ln {} td.src { "-" (t) } },
                                Line::Add(b, t) => tr.add id=[anchor.as_deref()] { td.ln {} td.ln { (cmt_link(v, f, key, i, *b)) } td.src { "+" (t) } },
                            }
                            @if let Some((side, n)) = key {
                                @let here: Vec<&InlineComment> = v.comments.iter().filter(|c| c.path == f.path && c.side == side && c.line == n as i64).collect();
                                @let open = v.at.is_some_and(|(p, s, ln)| p == f.path && s == side && ln == n as i64);
                                @if !here.is_empty() || open {
                                    tr.thread { td colspan="3" {
                                        @for c in &here {
                                            div.inline-comment id={ "rc" (c.id) } {
                                                div.small {
                                                    @match &c.author { Some(a) => a.who href={ "/" (a) } { (a) }, None => "ghost" }
                                                    " " span.muted { (ago(c.created)) }
                                                    @if c.can_delete {
                                                        @if let Some(act) = v.form_action {
                                                            " " form.inline method="post" action={ (act) "/comments/" (c.id) "/delete" } { button.link.danger { "delete" } }
                                                        }
                                                    }
                                                }
                                                div.prose { (c.html) }
                                            }
                                        }
                                        @if let Some(act) = v.form_action {
                                            @if open {
                                                form.form.inline-form method="post" action={ (act) "/comments" } {
                                                    input type="hidden" name="path" value=(f.path);
                                                    input type="hidden" name="side" value=(side);
                                                    input type="hidden" name="line" value=(n);
                                                    textarea.body name="body" rows="3" required autofocus placeholder="Comment on this line" {}
                                                    div.row {
                                                        button.btn.primary { "comment" }
                                                        a.btn href={ (act) "#" (anchor.as_deref().unwrap_or("")) } { "cancel" }
                                                    }
                                                }
                                            } @else {
                                                a.small href={ (act) "?at=" (crate::web::urlencode(&f.path)) ":" (side) ":" (n) "#" (anchor.as_deref().unwrap_or("")) } { "reply" }
                                            }
                                        }
                                    } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn cmt_link(v: &FilesView<'_>, f: &FileDiff, key: Option<(&str, u32)>, i: usize, n: u32) -> Markup {
    match (v.form_action, key) {
        (Some(act), Some((side, _))) => html! {
            a.add-cmt href={ (act) "?at=" (crate::web::urlencode(&f.path)) ":" (side) ":" (n) "#" (i) "-" (side) (n) } title="comment on this line" { (n) }
        },
        _ => html! { (n) },
    }
}

// ---------- revisions ----------

pub fn revisions(pbase: &str, revs: &[Revision], from: i64, to: i64, range_diff: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        (flash_box(err, None))
        div.card.flush {
            div.card-head { "revisions" span.muted { "each push to the head branch" } }
            ul.rows {
                @for r in revs.iter().rev() {
                    li {
                        strong { "v" (r.n) }
                        code.sha { (&r.head_sha[..r.head_sha.len().min(10)]) }
                        @if r.forced { span.badge.warn { "force-pushed" } }
                        span.grow {}
                        span.muted.small { @if let Some(p) = &r.pusher { (p) " · " } (ago(r.created)) }
                    }
                }
            }
        }
        @if revs.len() > 1 {
            form.card.compare-form method="get" action={ (pbase) "/revisions" } {
                span.muted { "range-diff" }
                select name="from" { @for r in revs { option value=(r.n) selected[r.n == from] { "v" (r.n) } } }
                span.muted { "→" }
                select name="to" { @for r in revs { option value=(r.n) selected[r.n == to] { "v" (r.n) } } }
                button.btn { "compare" }
            }
            @if let Some(rd) = range_diff {
                div.card.flush {
                    div.card-head { "v" (from) " → v" (to) }
                    @if rd.trim().is_empty() { p.pad.muted { "No differences." } } @else {
                        pre.range-diff {
                            @for l in rd.lines() {
                                @let t = l.trim_start();
                                @let class = if t.starts_with("+ ") || t.starts_with("++") { "add" } else if t.starts_with("- ") || t.starts_with("--") { "del" } else if l.contains(": ") && l.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-') { "hdr" } else { "" };
                                span class=(class) { (l) } "\n"
                            }
                        }
                    }
                }
            }
        }
    }
}
