//! Views: notifications, activity, dashboard, search, webhooks, statuses.

use maud::{html, Markup};

use crate::db::Repo;
use crate::social_db::{Activity, Delivery, Notification, SearchHit, Status, Webhook, HOOK_EVENTS};
use crate::ui::{ago, flash_box};

pub fn notifications(list: &[Notification], all: bool) -> Markup {
    html! {
        div.issue-bar {
            h1.page-title { "notifications" }
            nav.seg {
                a.active[!all] href="/notifications" { "unread" }
                a.active[all] href="/notifications?all=1" { "all" }
            }
            span.grow {}
            @if list.iter().any(|n| n.unread) {
                form.inline method="post" action="/notifications/read" { button.btn { "mark all read" } }
            }
        }
        div.card.flush {
            @if list.is_empty() { p.pad.muted { @if all { "Nothing yet." } @else { "All caught up." } } }
            ul.issues {
                @for n in list {
                    li.notif.unread[n.unread] {
                        span.state class=@if n.closed { "closed" } @else { "open" } { @if n.is_pr { "⇄" } @else { "●" } }
                        div.issue-main {
                            div {
                                span.muted { (n.repo) " " }
                                a.issue-title href={ "/" (n.repo) "/" @if n.is_pr { "pulls" } @else { "issues" } "/" (n.number) } { (n.title) }
                                span.muted { " #" (n.number) }
                            }
                            div.meta {
                                span.badge { (n.reason) }
                                @if let Some(a) = &n.actor { span.muted { "by " (a) } }
                                span.muted { (ago(n.updated)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn activity(items: &[Activity]) -> Markup {
    html! {
        div.card.flush {
            div.card-head { "activity" }
            @if items.is_empty() { p.pad.muted { "No activity yet." } }
            ul.feed {
                @for a in items {
                    @let mut f = a.data.split('\t');
                    @let (x, y, z, w) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                    li {
                        @match &a.actor { Some(n) => a.who href={ "/" (n) } { (n) }, None => span.who { "someone" } }
                        " "
                        @match a.kind.as_str() {
                            "push" => {
                                @if w == "deleted" { "deleted branch " code { (x) } " in " }
                                @else { "pushed " (y) " commit" @if y != "1" { "s" } " to " a href={ "/" (a.repo) "/tree/" (x) } { code { (x) } } " in " }
                            },
                            "issue" => { (z) " issue " a href={ "/" (a.repo) "/issues/" (x) } { "#" (x) " " (y) } " in " },
                            "comment" => { "commented on " a href={ "/" (a.repo) "/" @if z == "pr" { "pulls" } @else { "issues" } "/" (x) } { "#" (x) " " (y) } " in " },
                            "pull" => { (z) " pull request " a href={ "/" (a.repo) "/pulls/" (x) } { "#" (x) " " (y) } " in " },
                            "review" => {
                                @match z { "approved" => "approved ", "changes_requested" => "requested changes on ", _ => "reviewed " }
                                a href={ "/" (a.repo) "/pulls/" (x) } { "#" (x) " " (y) } " in "
                            },
                            _ => { (a.kind) " in " },
                        }
                        a href={ "/" (a.repo) } { (a.repo) }
                        " " span.muted { (ago(a.created)) }
                    }
                }
            }
        }
    }
}

pub fn dashboard(name: &str, items: &[Activity], repos: &[Repo]) -> Markup {
    html! {
        h1.page-title { "hi, " (name) }
        div.split {
            div { (activity(items)) }
            aside.card.flush {
                div.card-head { "repositories" span.grow {} a href="/new" { "new" } }
                ul.rows {
                    @for r in repos {
                        li { a href={ "/" (r.owner) "/" (r.name) } { span.muted { (r.owner) "/" } (r.name) } @if r.private { span.badge { "private" } } }
                    }
                }
            }
        }
    }
}

pub struct CodeHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

pub struct SearchData<'a> {
    pub q: &'a str,
    pub kind: &'a str,
    /// Repo-scoped search (owner/name), or global.
    pub scope: Option<&'a str>,
    pub repos: &'a [Repo],
    pub issues: &'a [SearchHit],
    pub code: &'a [CodeHit],
    pub code_rev: &'a str,
    pub err: Option<&'a str>,
}

/// FTS snippets mark matches with [ ]; render them as <mark>.
fn snippet(s: &str) -> Markup {
    let mut parts = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        let Some(end) = rest[start..].find(']') else { break };
        parts.push((false, &rest[..start]));
        parts.push((true, &rest[start + 1..start + end]));
        rest = &rest[start + end + 1..];
    }
    parts.push((false, rest));
    html! { @for (hit, t) in parts { @if hit { mark { (t) } } @else { (t) } } }
}

pub fn search(d: &SearchData<'_>) -> Markup {
    let action = match d.scope { Some(r) => format!("/{r}/search"), None => "/search".into() };
    let tab = |k: &str| format!("{action}?q={}&type={k}", crate::web::urlencode(d.q));
    html! {
        form.card.compare-form method="get" action=(action) {
            input type="search" name="q" value=(d.q) placeholder="search" autofocus;
            input type="hidden" name="type" value=(d.kind);
            button.btn.primary { "search" }
        }
        nav.tabs.mt {
            @if d.scope.is_none() { a.active[d.kind == "repos"] href=(tab("repos")) { "repositories" } }
            a.active[d.kind == "issues"] href=(tab("issues")) { "issues & pulls" }
            @if d.scope.is_some() { a.active[d.kind == "code"] href=(tab("code")) { "code" } }
        }
        (flash_box(d.err, None))
        div.card.flush.mt {
            @match d.kind {
                "repos" => {
                    @if d.repos.is_empty() { p.pad.muted { "No repositories match." } }
                    ul.rows { @for r in d.repos {
                        li { div { a href={ "/" (r.owner) "/" (r.name) } { span.muted { (r.owner) "/" } strong { (r.name) } } div.muted.small { (r.description) } } }
                    } }
                },
                "code" => {
                    @if d.code.is_empty() { p.pad.muted { "No matches in " code { (d.code_rev) } "." } }
                    ul.rows.code-hits { @for h in d.code {
                        li {
                            a.mono href={ "/" (d.scope.unwrap_or("")) "/blob/" (d.code_rev) "/" (h.path) "#L" (h.line) } { (h.path) ":" (h.line) }
                            code.hit { (h.text) }
                        }
                    } }
                },
                _ => {
                    @if d.issues.is_empty() { p.pad.muted { "No issues or pull requests match." } }
                    ul.issues { @for h in d.issues {
                        li {
                            span.state class=@if h.closed { "closed" } @else { "open" } { @if h.is_pr { "⇄" } @else { "●" } }
                            div.issue-main {
                                div {
                                    @if d.scope.is_none() { span.muted { (h.repo) " " } }
                                    a.issue-title href={ "/" (h.repo) "/" @if h.is_pr { "pulls" } @else { "issues" } "/" (h.number) } { (h.title) }
                                    span.muted { " #" (h.number) }
                                }
                                div.muted.small { (snippet(&h.snippet)) }
                            }
                        }
                    } }
                },
            }
        }
    }
}

// ---------- webhooks ----------

pub fn hooks_section(sbase: &str, hooks: &[(Webhook, Option<(Option<i64>, bool)>)]) -> Markup {
    html! {
        section.card.flush {
            div.card-head { "webhooks" span.muted { (hooks.len()) } }
            @if hooks.is_empty() { p.pad.muted { "No webhooks. Deliveries are signed with your secret (X-Hub-Signature-256)." } }
            ul.rows {
                @for (h, last) in hooks {
                    li {
                        @match last {
                            Some((Some(s), _)) if (200..300).contains(s) => span.verdict.ok title="last delivery ok" { "✓" },
                            Some((Some(_), _)) | Some((None, true)) => span.verdict.bad title="last delivery failed" { "✕" },
                            Some((None, false)) => span.muted title="pending" { "…" },
                            None => span.muted { "·" },
                        }
                        a.mono href={ (sbase) "/hooks/" (h.id) } { (h.url) }
                        span.muted.small { (h.events) }
                        span.grow {}
                        form.inline method="post" action={ (sbase) "/hooks/" (h.id) "/test" } { button.link { "ping" } }
                        form.inline method="post" action={ (sbase) "/hooks/" (h.id) "/delete" } { button.link.danger { "delete" } }
                    }
                }
            }
            form.row-form method="post" action={ (sbase) "/hooks" } {
                input name="url" placeholder="https://ci.example.com/hook" required maxlength="2000";
                input name="secret" placeholder="secret (recommended)" maxlength="200" autocomplete="off";
                @for e in HOOK_EVENTS { label.check { input type="checkbox" name="event" value=(e) checked; (e) } }
                button.btn { "add webhook" }
            }
        }
    }
}

pub fn hook_page(sbase: &str, h: &Webhook, deliveries: &[Delivery], msg: Option<&str>) -> Markup {
    html! {
        div.stack {
            (flash_box(None, msg))
            div.card {
                h3.mono { (h.url) }
                p.muted.small { "events: " (h.events) " · " @if h.secret.is_empty() { "unsigned" } @else { "signed" } }
                form.inline method="post" action={ (sbase) "/hooks/" (h.id) "/test" } { button.btn { "send ping" } }
            }
            div.card.flush {
                div.card-head { "recent deliveries" span.muted { (deliveries.len()) } }
                @if deliveries.is_empty() { p.pad.muted { "None yet." } }
                ul.rows {
                    @for d in deliveries {
                        li.delivery {
                            details {
                                summary {
                                    @match (d.status, d.done) {
                                        (Some(s), _) if (200..300).contains(&s) => span.verdict.ok { (s) },
                                        (Some(s), _) => span.verdict.bad { (s) },
                                        (None, true) => span.verdict.bad { "failed" },
                                        (None, false) => span.muted { "pending" },
                                    }
                                    " " code { (d.event) } " "
                                    span.muted.small {
                                        (ago(d.created)) " · " (d.attempts) " attempt" @if d.attempts != 1 { "s" }
                                        @if let Some(ms) = d.duration_ms { " · " (ms) " ms" }
                                    }
                                }
                                div.delivery-body {
                                    h4 { "payload" }
                                    pre { (d.payload) }
                                    @if !d.response.is_empty() { h4 { "response" } pre { (d.response) } }
                                    form.inline method="post" action={ (sbase) "/hooks/" (h.id) "/deliveries/" (d.id) "/redeliver" } { button.btn { "redeliver" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------- statuses ----------

fn status_icon(state: &str) -> Markup {
    html! {
        @match state {
            "success" => span.verdict.ok title="success" { "✓" },
            "pending" => span.verdict.pending title="pending" { "●" },
            _ => span.verdict.bad title=(state) { "✕" },
        }
    }
}

/// Status list; `required` contexts missing from `list` show as expected.
pub fn statuses(list: &[Status], required: &[String]) -> Markup {
    let missing: Vec<&String> = required.iter().filter(|r| !list.iter().any(|s| &s.context == *r)).collect();
    html! {
        @if !list.is_empty() || !missing.is_empty() {
            div.checks {
                @for s in list {
                    div.check {
                        (status_icon(&s.state))
                        strong { (s.context) }
                        @if required.contains(&s.context) { span.badge { "required" } }
                        span.muted.small { (s.description) }
                        span.grow {}
                        @if !s.target_url.is_empty() { a.small href=(s.target_url) rel="noopener noreferrer" { "details" } }
                    }
                }
                @for r in missing {
                    div.check { span.verdict.pending { "○" } strong { (r) } span.badge { "required" } span.muted.small { "expected — waiting for status" } }
                }
            }
        }
    }
}
