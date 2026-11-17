//! Views for issues, labels and milestones.

use maud::{html, Markup};

use crate::issues_db::{Comment, Filter, Issue, Label, Milestone};
use crate::ui::{ago, flash_box};

/// Readable text color for a validated `#rrggbb` background.
fn ink(hex: &str) -> &'static str {
    let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f32;
    let lum = 0.299 * c(1) + 0.587 * c(3) + 0.114 * c(5);
    if lum > 150.0 { "#111" } else { "#fff" }
}

pub fn label_chip(l: &Label, href: Option<&str>) -> Markup {
    let style = format!("background:{};color:{}", l.color, ink(&l.color));
    html! {
        @if let Some(h) = href {
            a.label href=(h) style=(style) title=(l.description) { (l.name) }
        } @else {
            span.label style=(style) title=(l.description) { (l.name) }
        }
    }
}

fn state_icon(i: &Issue) -> Markup {
    html! {
        @if i.merged { span.state.merged title="merged" { "⇄" } }
        @else if i.closed { span.state.closed title="closed" { "✓" } }
        @else { span.state.open title="open" { "●" } }
    }
}

pub fn state_pill(closed: bool) -> Markup {
    html! { @if closed { span.pill.closed { "✓ closed" } } @else { span.pill.open { "● open" } } }
}

pub fn pr_state_pill(closed: bool, merged: bool) -> Markup {
    html! {
        @if merged { span.pill.merged { "⇄ merged" } }
        @else if closed { span.pill.closed { "✕ closed" } }
        @else { span.pill.open { "● open" } }
    }
}

/// Query string for the list view, overriding one key.
pub fn list_query(f: &Filter, closed: Option<bool>, page: Option<usize>) -> String {
    let mut q = vec![format!("state={}", if closed.unwrap_or(f.closed) { "closed" } else { "open" })];
    let enc = crate::web::urlencode;
    if let Some(v) = &f.label { q.push(format!("label={}", enc(v))) }
    if let Some(v) = &f.milestone { q.push(format!("milestone={}", enc(v))) }
    if let Some(v) = &f.assignee { q.push(format!("assignee={}", enc(v))) }
    if let Some(v) = &f.author { q.push(format!("author={}", enc(v))) }
    if let Some(p) = page.filter(|p| *p > 0) { q.push(format!("page={p}")) }
    format!("?{}", q.join("&"))
}

pub struct ListData<'a> {
    /// "issues" or "pulls"
    pub kind: &'a str,
    pub base: &'a str,
    pub issues: &'a [Issue],
    pub filter: &'a Filter,
    pub open: i64,
    pub closed: i64,
    pub page: usize,
    pub more: bool,
    pub can_create: bool,
}

pub fn list(d: &ListData<'_>) -> Markup {
    let f = d.filter;
    let active = [("label", &f.label), ("milestone", &f.milestone), ("assignee", &f.assignee), ("author", &f.author)];
    html! {
        div.issue-bar {
            nav.seg {
                a.active[!f.closed] href={ (list_query(f, Some(false), None)) } { "● " (d.open) " open" }
                a.active[f.closed] href={ (list_query(f, Some(true), None)) } { "✓ " (d.closed) " closed" }
            }
            @for (k, v) in active {
                @if let Some(v) = v { a.filter-chip href={ (d.base) "/" (d.kind) "?state=" @if f.closed { "closed" } @else { "open" } } title="clear filter" { (k) ": " (v) " ×" } }
            }
            span.grow {}
            a.btn href={ (d.base) "/labels" } { "labels" }
            a.btn href={ (d.base) "/milestones" } { "milestones" }
            @if d.can_create {
                @if d.kind == "pulls" { a.btn.primary href={ (d.base) "/compare" } { "new pull request" } }
                @else { a.btn.primary href={ (d.base) "/issues/new" } { "new issue" } }
            }
        }
        div.card.flush {
            @if d.issues.is_empty() { p.pad.muted { "No " @if f.closed { "closed" } @else { "open" } " " @if d.kind == "pulls" { "pull requests" } @else { "issues" } "." } }
            ul.issues {
                @for i in d.issues {
                    li {
                        (state_icon(i))
                        div.issue-main {
                            div {
                                a.issue-title href={ (d.base) "/" (d.kind) "/" (i.number) } { (i.title) }
                                @if i.draft { " " span.badge.warn { "draft" } }
                                @for l in &i.labels { " " (label_chip(l, Some(&format!("{}/{}?label={}", d.base, d.kind, crate::web::urlencode(&l.name))))) }
                            }
                            div.meta {
                                span.muted { "#" (i.number) }
                                span.muted {
                                    @if i.merged { "merged " (ago(i.closed_at.unwrap_or(i.updated))) }
                                    @else if i.closed { "closed " (ago(i.closed_at.unwrap_or(i.updated))) }
                                    @else { "opened " (ago(i.created)) }
                                    " by "
                                    @match &i.author { Some(a) => a href={ (d.base) "/" (d.kind) "?author=" (a) } { (a) }, None => "ghost" }
                                }
                                @if let Some(m) = &i.milestone { a.muted href={ (d.base) "/" (d.kind) "?milestone=" (crate::web::urlencode(m)) } { "◇ " (m) } }
                            }
                        }
                        @if i.comments > 0 { a.comments href={ (d.base) "/" (d.kind) "/" (i.number) } { "💬 " (i.comments) } }
                    }
                }
            }
        }
        nav.pager {
            @if d.page > 0 { a href={ (list_query(f, None, Some(d.page - 1))) } { "← newer" } }
            @if d.more { a href={ (list_query(f, None, Some(d.page + 1))) } { "older →" } }
        }
    }
}

pub fn new_issue(base: &str, title: &str, body: &str, err: Option<&str>) -> Markup {
    html! {
        div.stack {
            (flash_box(err, None))
            form.card.form method="post" action={ (base) "/issues/new" } {
                h3 { "new issue" }
                label { "title" input name="title" value=(title) required maxlength="256" autofocus; }
                label { "description" textarea.body name="body" rows="10" placeholder="Markdown. Reference #123, owner/repo#4, @user or a commit sha." { (body) } }
                button.btn.primary { "open issue" }
            }
        }
    }
}

pub enum Item {
    Comment { c: Comment, html: Markup, can_edit: bool, can_delete: bool },
    Event { actor: Option<String>, created: i64, text: Markup },
    Review { author: Option<String>, state: String, html: Markup, created: i64 },
    Code { author: Option<String>, path: String, line: i64, html: Markup, created: i64, outdated: bool, link: String },
}

pub struct ShowData<'a> {
    pub kind: &'a str,
    pub base: &'a str,
    pub issue: &'a Issue,
    pub body_html: Markup,
    pub items: Vec<Item>,
    pub assignees: Vec<String>,
    pub labels: Vec<Label>,
    pub milestones: Vec<Milestone>,
    pub signed_in: bool,
    pub can_comment: bool,
    pub can_edit: bool,
    pub can_triage: bool,
    pub can_close: bool,
    pub archived: bool,
    /// (token, count, mine) — the viewer's emoji reactions on the issue itself.
    pub reactions: Vec<(String, i64, bool)>,
    pub msg: Option<&'a str>,
    pub err: Option<&'a str>,
    /// PR pages: replaces the issue title block (title, state, sub-tabs).
    pub head: Option<Markup>,
    /// PR pages: merge box above the comment form.
    pub merge_box: Option<Markup>,
    /// PR pages: reviewers etc. at the top of the sidebar.
    pub extra_side: Option<Markup>,
}

pub fn reaction_emoji(name: &str) -> &'static str {
    match name {
        "+1" => "👍",
        "-1" => "👎",
        "laugh" => "😄",
        "confused" => "😕",
        "heart" => "❤️",
        "hooray" => "🎉",
        "rocket" => "🚀",
        _ => "👀",
    }
}

fn reactions_row(ibase: &str, d: &ShowData<'_>) -> Markup {
    let can_react = d.signed_in && !d.archived;
    html! {
        @if !d.reactions.is_empty() || can_react {
            div.reactions {
                @for (name, count, mine) in &d.reactions {
                    form.inline method="post" action={ (ibase) "/react" } {
                        input type="hidden" name="name" value=(name);
                        @if can_react {
                            button.reaction class=[(*mine).then_some("mine")] title="toggle" { (reaction_emoji(name)) " " (count) }
                        } @else {
                            span.reaction class=[(*mine).then_some("mine")] { (reaction_emoji(name)) " " (count) }
                        }
                    }
                }
                @if can_react {
                    form.inline method="post" action={ (ibase) "/react" } {
                        select name="name" aria-label="add reaction" {
                            @for n in crate::issues_db::REACTIONS {
                                @if !d.reactions.iter().any(|(x, _, _)| x == n) { option value=(n) { (reaction_emoji(n)) } }
                            }
                        }
                        button.btn.small-btn { "+" }
                    }
                }
            }
        }
    }
}

fn who(_base: &str, name: &Option<String>) -> Markup {
    html! { @match name { Some(n) => (crate::ui::user_link(n, 20)), None => span.who { "ghost" } } }
}

pub fn show(d: &ShowData<'_>) -> Markup {
    let i = d.issue;
    let ibase = format!("{}/{}/{}", d.base, d.kind, i.number);
    html! {
        @if let Some(h) = &d.head { (h) } @else {
            div.issue-head {
                h2 { (i.title) " " span.muted { "#" (i.number) } }
                div.meta {
                    (state_pill(i.closed))
                    span.muted { (who(d.base, &i.author)) " opened this " (ago(i.created)) " · " (i.comments) " comments" }
                }
            }
        }
        (flash_box(d.err, d.msg))
        div.split {
            div.stack.timeline {
                article.card.flush.comment {
                    div.card-head {
                        (who(d.base, &i.author)) span.muted { (ago(i.created)) }
                        @if i.updated > i.created + 60 && !i.body.is_empty() { span.muted { "· edited" } }
                        span.grow {}
                        @if d.can_edit {
                            details.edit {
                                summary.link { "edit" }
                                form.form method="post" action={ (ibase) "/edit" } {
                                    input name="title" value=(i.title) required maxlength="256";
                                    textarea.body name="body" rows="8" { (i.body) }
                                    button.btn.primary { "save" }
                                }
                            }
                        }
                    }
                    div.prose.pad { @if i.body.is_empty() { p.muted { em { "No description." } } } @else { (d.body_html) } }
                }
                (reactions_row(&ibase, d))
                @for item in &d.items {
                    @match item {
                        Item::Comment { c, html, can_edit, can_delete } => {
                            article.card.flush.comment id={ "c" (c.id) } {
                                div.card-head {
                                    (who(d.base, &c.author))
                                    a.muted href={ "#c" (c.id) } { (ago(c.created)) }
                                    @if c.updated > c.created + 60 { span.muted { "· edited" } }
                                    span.grow {}
                                    @if *can_edit {
                                        details.edit {
                                            summary.link { "edit" }
                                            form.form method="post" action={ (d.base) "/comments/" (c.id) "/edit" } {
                                                textarea.body name="body" rows="6" required { (c.body) }
                                                button.btn.primary { "save" }
                                            }
                                        }
                                    }
                                    @if *can_delete {
                                        form.inline method="post" action={ (d.base) "/comments/" (c.id) "/delete" } { button.link.danger { "delete" } }
                                    }
                                }
                                div.prose.pad { (html) }
                            }
                        }
                        Item::Event { actor, created, text } => {
                            div.event { span.dot {} (who(d.base, actor)) " " (text) " " span.muted { (ago(*created)) } }
                        }
                        Item::Review { author, state, html, created } => {
                            article class={ "card flush comment review review-" (state) } {
                                div.card-head {
                                    (who(d.base, author))
                                    @match state.as_str() {
                                        "approve" => span.verdict.ok { "✓ approved" },
                                        "changes" => span.verdict.bad { "✕ requested changes" },
                                        _ => span.verdict { "reviewed" },
                                    }
                                    span.muted { (ago(*created)) }
                                }
                                @if !html.0.is_empty() { div.prose.pad { (html) } }
                            }
                        }
                        Item::Code { author, path, line, html, created, outdated, link } => {
                            article.card.flush.comment.code-comment {
                                div.card-head {
                                    (who(d.base, author))
                                    span.muted { "on " }
                                    a.mono href=(link) { (path) ":" (line) }
                                    @if *outdated { span.badge { "outdated" } }
                                    span.muted { (ago(*created)) }
                                }
                                div.prose.pad { (html) }
                            }
                        }
                    }
                }
                @if let Some(m) = &d.merge_box { (m) }
                @if d.can_comment {
                    form.card.form method="post" action={ (ibase) "/comments" } {
                        textarea.body name="body" rows="6" placeholder="Leave a comment (markdown)" {}
                        div.row {
                            button.btn.primary name="action" value="comment" { "comment" }
                            @if d.can_close {
                                @if i.closed {
                                    button.btn name="action" value="reopen" { "reopen" }
                                } @else {
                                    button.btn name="action" value="close" { @if d.kind == "pulls" { "close pull request" } @else { "close issue" } }
                                }
                            }
                        }
                    }
                } @else if d.archived {
                    p.muted { "This repository is archived; the conversation is locked." }
                } @else if !d.signed_in {
                    p.muted { a href={ "/login?next=" (crate::web::urlencode(&ibase)) } { "Sign in" } " to comment." }
                }
            }
            aside.stack.sidebar {
                @if let Some(x) = &d.extra_side { (x) }
                section.card {
                    h4 { "assignees" }
                    @if d.assignees.is_empty() { p.muted.small { "nobody" } }
                    @for a in &d.assignees {
                        div.side-row {
                            a href={ "/" (a) } { (a) }
                            @if d.can_triage {
                                form.inline method="post" action={ (ibase) "/assignees" } {
                                    input type="hidden" name="remove" value=(a);
                                    button.link title="unassign" { "×" }
                                }
                            }
                        }
                    }
                    @if d.can_triage {
                        form.side-form method="post" action={ (ibase) "/assignees" } {
                            input name="add" placeholder="user" required;
                            button.btn { "+" }
                        }
                    }
                }
                section.card {
                    h4 { "labels" }
                    @if d.can_triage {
                        form.label-pick method="post" action={ (ibase) "/labels" } {
                            @for l in &d.labels {
                                label.check {
                                    input type="checkbox" name="label" value=(l.id) checked[i.labels.iter().any(|x| x.id == l.id)];
                                    (label_chip(l, None))
                                }
                            }
                            button.btn { "apply" }
                        }
                    } @else {
                        @if i.labels.is_empty() { p.muted.small { "none" } }
                        div.chips { @for l in &i.labels { (label_chip(l, None)) " " } }
                    }
                }
                section.card {
                    h4 { "milestone" }
                    @if d.can_triage {
                        form.side-form method="post" action={ (ibase) "/milestone" } {
                            select name="milestone" {
                                option value="" { "none" }
                                @for m in d.milestones.iter().filter(|m| !m.closed || Some(m.id) == i.milestone_id) {
                                    option value=(m.id) selected[Some(m.id) == i.milestone_id] { (m.title) }
                                }
                            }
                            button.btn { "set" }
                        }
                    } @else {
                        p.small { @match &i.milestone { Some(m) => (m), None => span.muted { "none" } } }
                    }
                }
            }
        }
    }
}

pub fn labels_page(base: &str, labels: &[Label], counts: &[(i64, i64)], can_edit: bool, msg: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        div.stack {
            (flash_box(err, msg))
            div.card.flush {
                div.card-head { "labels" span.muted { (labels.len()) } }
                ul.rows {
                    @for l in labels {
                        @let n = counts.iter().find(|(id, _)| *id == l.id).map_or(0, |c| c.1);
                        li {
                            (label_chip(l, Some(&format!("{base}/issues?label={}", crate::web::urlencode(&l.name)))))
                            span.muted.small { (l.description) }
                            span.grow {}
                            span.muted.small { (n) " open" }
                            @if can_edit {
                                details.edit {
                                    summary.link { "edit" }
                                    form.row-form method="post" action={ (base) "/labels/" (l.id) } {
                                        input name="name" value=(l.name) required maxlength="50";
                                        input type="color" name="color" value=(l.color);
                                        input name="description" value=(l.description) maxlength="200";
                                        button.btn { "save" }
                                    }
                                }
                                form.inline method="post" action={ (base) "/labels/" (l.id) "/delete" } { button.link.danger { "delete" } }
                            }
                        }
                    }
                }
                @if can_edit {
                    form.row-form method="post" action={ (base) "/labels" } {
                        input name="name" placeholder="new label" required maxlength="50";
                        input type="color" name="color" value="#6b7280";
                        input name="description" placeholder="description" maxlength="200";
                        button.btn { "create" }
                    }
                }
            }
        }
    }
}

fn date_value(t: Option<i64>) -> String {
    t.map(|t| {
        let days = t.div_euclid(86400);
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}")
    })
    .unwrap_or_default()
}

/// Howard Hinnant's days -> civil date.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` -> unix seconds (UTC midnight).
pub fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || it.next().is_some() {
        return None;
    }
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400)
}

pub fn milestones_page(base: &str, ms: &[Milestone], can_edit: bool, msg: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        div.stack {
            (flash_box(err, msg))
            div.card.flush {
                div.card-head { "milestones" span.muted { (ms.len()) } }
                @if ms.is_empty() { p.pad.muted { "No milestones." } }
                ul.rows {
                    @for m in ms {
                        @let total = m.open_issues + m.closed_issues;
                        @let pct = if total == 0 { 0 } else { m.closed_issues * 100 / total };
                        li.milestone {
                            div.grow {
                                a href={ (base) "/issues?milestone=" (crate::web::urlencode(&m.title)) "&state=open" } { strong { (m.title) } }
                                @if m.closed { span.badge { "closed" } }
                                @if let Some(due) = m.due { span.muted.small { " · due " (date_value(Some(due))) } }
                                @if !m.description.is_empty() { div.muted.small { (m.description) } }
                                div.progress { div style={ "width:" (pct) "%" } {} }
                                div.muted.small { (pct) "% · " (m.open_issues) " open · " (m.closed_issues) " closed" }
                            }
                            @if can_edit {
                                details.edit {
                                    summary.link { "edit" }
                                    form.form method="post" action={ (base) "/milestones/" (m.id) } {
                                        input name="title" value=(m.title) required maxlength="100";
                                        input name="description" value=(m.description) maxlength="500";
                                        input type="date" name="due" value=(date_value(m.due));
                                        label.check { input type="checkbox" name="closed" value="1" checked[m.closed]; "closed" }
                                        button.btn { "save" }
                                    }
                                }
                                form.inline method="post" action={ (base) "/milestones/" (m.id) "/delete" } { button.link.danger { "delete" } }
                            }
                        }
                    }
                }
                @if can_edit {
                    form.row-form method="post" action={ (base) "/milestones" } {
                        input name="title" placeholder="new milestone" required maxlength="100";
                        input name="description" placeholder="description" maxlength="500";
                        input type="date" name="due";
                        button.btn { "create" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_roundtrip() {
        for s in ["1970-01-01", "2000-02-29", "2026-11-16", "2099-12-31"] {
            assert_eq!(date_value(parse_date(s)), s);
        }
        assert!(parse_date("2026-13-01").is_none());
        assert!(parse_date("x").is_none());
    }
}
