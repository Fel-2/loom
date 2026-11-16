//! HTML views. Server-rendered, no JS required.

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::db::{Repo, SshKey, Token, User};
use crate::git::{CommitInfo, Entry, FileDiff, Line, RefInfo};

pub fn page(title: &str, viewer: Option<&User>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · loom" }
                link rel="stylesheet" href="/static/style.css";
            }
            body {
                header.top {
                    a.brand href="/" { "loom" }
                    span.grow {}
                    @if let Some(u) = viewer {
                        a.btn href="/new" { "+ new" }
                        a href="/settings/keys" { (u.name) }
                        form.inline method="post" action="/logout" { button.link { "sign out" } }
                    } @else {
                        a href="/login" { "sign in" }
                    }
                }
                main { (body) }
            }
        }
    }
}

pub fn ago(t: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let d = (now - t).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86400 => (d / 3600, "hour"),
        86400..2_592_000 => (d / 86400, "day"),
        2_592_000..31_536_000 => (d / 2_592_000, "month"),
        _ => (d / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn short(id: &gix::ObjectId) -> String {
    id.to_hex_with_len(8).to_string()
}

pub fn index(repos: &[Repo], owner: Option<&str>) -> Markup {
    html! {
        h1.page-title { @if let Some(o) = owner { (o) } @else { "repositories" } }
        @if repos.is_empty() {
            p.muted { "No repositories yet." }
        }
        ul.cards {
            @for r in repos {
                li.card {
                    a.repo-link href={ "/" (r.owner) "/" (r.name) } {
                        span.muted { (r.owner) " / " } strong { (r.name) }
                    }
                    @if r.private { span.badge { "private" } }
                    @if !r.description.is_empty() { p.muted { (r.description) } }
                }
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Overview,
    Commits,
    Refs,
}

pub struct Clone<'a> {
    pub http: &'a str,
    pub ssh: Option<&'a str>,
}

pub fn repo_header(repo: &Repo, clone: &Clone<'_>, tab: Tab, rev: &str) -> Markup {
    let (owner, name) = (&repo.owner, &repo.name);
    let base = format!("/{owner}/{name}");
    html! {
        section.repo-head {
            div.repo-title {
                a href={ "/" (owner) } { (owner) } span.sep { "/" } a href=(base) { strong { (name) } }
                @if repo.private { span.badge { "private" } }
            }
            @if !repo.description.is_empty() { p.muted { (repo.description) } }
            details.clone {
                summary { "clone" }
                div.card {
                    label { "https" } input readonly value=(clone.http);
                    @if let Some(s) = clone.ssh { label { "ssh" } input readonly value=(s); }
                }
            }
            nav.tabs {
                a.active[tab == Tab::Overview] href=(base) { "overview" }
                a.active[tab == Tab::Commits] href={ (base) "/log/" (rev) } { "commits" }
                a.active[tab == Tab::Refs] href={ (base) "/refs" } { "branches & tags" }
            }
        }
    }
}

/// Breadcrumb for `path` within a repo at `rev`.
pub fn crumbs(base: &str, rev: &str, path: &str) -> Markup {
    let mut acc = String::new();
    html! {
        div.crumbs {
            span.ref-pill { (rev) }
            a href={ (base) "/tree/" (rev) } { "root" }
            @for seg in path.split('/').filter(|s| !s.is_empty()) {
                ({ acc.push('/'); acc.push_str(seg); "" })
                span.sep { "/" }
                a href={ (base) "/tree/" (rev) (acc) } { (seg) }
            }
        }
    }
}

pub fn tree(base: &str, rev: &str, path: &str, entries: &[Entry]) -> Markup {
    let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
    html! {
        div.card.flush {
            ul.files {
                @if !path.is_empty() {
                    @let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
                    li { a href={ (base) "/tree/" (rev) "/" (parent) } { span.icon { "↰" } ".." } }
                }
                @for e in entries {
                    li {
                        @if e.is_submodule {
                            span { span.icon { "◇" } (e.name) span.muted { " submodule" } }
                        } @else if e.is_dir {
                            a.dir href={ (base) "/tree/" (rev) "/" (prefix) (e.name) } { span.icon { "▸" } (e.name) "/" }
                        } @else {
                            a href={ (base) "/blob/" (rev) "/" (prefix) (e.name) } { span.icon { "·" } (e.name) }
                        }
                    }
                }
            }
        }
    }
}

pub fn commit_rows(base: &str, commits: &[CommitInfo]) -> Markup {
    html! {
        ul.commits {
            @for c in commits {
                li {
                    a.summary href={ (base) "/commit/" (c.id) } { (c.summary) }
                    div.meta {
                        span { (c.author) }
                        span.muted title=(c.time) { (ago(c.time)) }
                        a.sha href={ (base) "/commit/" (c.id) } { (short(&c.id)) }
                    }
                }
            }
        }
    }
}

pub fn markdown(src: &str) -> Markup {
    use pulldown_cmark::{html::push_html, Event, Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    // Raw HTML in READMEs is rendered as text: no sanitizer dependency, no XSS.
    let events = Parser::new_ext(src, opts).map(|e| match e {
        Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
        e => e,
    });
    let mut out = String::with_capacity(src.len() * 3 / 2);
    push_html(&mut out, events);
    PreEscaped(out)
}

pub fn readme(name: &str, content: &str) -> Markup {
    let is_md = name.to_ascii_lowercase().ends_with(".md") || name.eq_ignore_ascii_case("readme");
    html! {
        article.card.readme {
            div.card-head { (name) }
            @if is_md { div.prose { (markdown(content)) } } @else { pre { (content) } }
        }
    }
}

pub fn blob(base: &str, rev: &str, path: &str, data: &[u8]) -> Markup {
    let binary = data.iter().take(8000).any(|&b| b == 0);
    let text = String::from_utf8_lossy(data);
    html! {
        div.card.flush {
            div.card-head {
                span { (path.rsplit('/').next().unwrap_or(path)) }
                span.muted { (data.len()) " bytes" }
                span.grow {}
                a href={ (base) "/log/" (rev) "?path=" (path) } { "history" }
                a href={ (base) "/raw/" (rev) "/" (path) } { "raw" }
            }
            @if binary {
                p.pad.muted { "Binary file not shown." }
            } @else {
                table.code {
                    @for (i, l) in text.lines().enumerate() {
                        tr id={ "L" (i + 1) } {
                            td.ln { a href={ "#L" (i + 1) } { (i + 1) } }
                            td.src { (l) }
                        }
                    }
                }
            }
        }
    }
}

pub fn commit(base: &str, c: &CommitInfo, files: &[FileDiff]) -> Markup {
    let (adds, dels) = files.iter().fold((0, 0), |(a, d), f| (a + f.adds, d + f.dels));
    let (_, rest) = c.message.split_once('\n').unwrap_or((&c.message, ""));
    html! {
        section.card.commit-head {
            h2 { (c.summary) }
            @if !rest.trim().is_empty() { pre.msg { (rest.trim()) } }
            div.meta {
                span { (c.author) } span.muted { "<" (c.email) ">" }
                span.muted { (ago(c.time)) }
                span.grow {}
                @for p in &c.parents { a.sha href={ (base) "/commit/" (p) } { "parent " (short(p)) } }
                a.sha href={ (base) "/tree/" (c.id) } { "browse " (short(&c.id)) }
            }
        }
        p.muted.stat {
            (files.len()) " files changed, "
            span.add { "+" (adds) } " " span.del { "-" (dels) }
        }
        nav.card.filelist {
            @for (i, f) in files.iter().enumerate() {
                a href={ "#f" (i) } {
                    span class={ "st st-" (f.status) } { (f.status) } (f.path)
                    span.grow {} span.add { "+" (f.adds) } span.del { "-" (f.dels) }
                }
            }
        }
        @for (i, f) in files.iter().enumerate() {
            div.card.flush.diff id={ "f" (i) } {
                div.card-head {
                    span class={ "st st-" (f.status) } { (f.status) }
                    @if let Some(o) = &f.old_path { span.muted { (o) " → " } }
                    a href={ (base) "/blob/" (c.id) "/" (f.path) } { (f.path) }
                }
                @if f.binary {
                    p.pad.muted { "Binary or large file not shown." }
                } @else {
                    table.code {
                        @for l in &f.lines {
                            @match l {
                                Line::Hunk(h) => tr.hunk { td.ln {} td.ln {} td.src { (h) } },
                                Line::Ctx(a, b, t) => tr { td.ln { (a) } td.ln { (b) } td.src { " " (t) } },
                                Line::Del(a, t) => tr.del { td.ln { (a) } td.ln {} td.src { "-" (t) } },
                                Line::Add(b, t) => tr.add { td.ln {} td.ln { (b) } td.src { "+" (t) } },
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn refs(base: &str, branches: &[RefInfo], tags: &[RefInfo], default: Option<&str>) -> Markup {
    let list = |title: &str, items: &[RefInfo]| {
        html! {
            section.card.flush {
                div.card-head { (title) span.muted { (items.len()) } }
                ul.commits {
                    @for r in items {
                        li {
                            a.summary href={ (base) "/tree/" (r.name) } {
                                (r.name)
                                @if Some(r.name.as_str()) == default { span.ref-pill { "default" } }
                            }
                            div.meta {
                                span.muted { (r.summary) }
                                span.muted { (ago(r.time)) }
                                a.sha href={ (base) "/commit/" (r.target) } { (short(&r.target)) }
                            }
                        }
                    }
                }
            }
        }
    };
    html! { div.stack { (list("branches", branches)) (list("tags", tags)) } }
}

pub fn empty_repo(clone: &Clone<'_>) -> Markup {
    let remote = clone.ssh.unwrap_or(clone.http);
    html! {
        div.card {
            h3 { "Empty repository" }
            p.muted { "Push an existing repository:" }
            pre { "git remote add origin " (remote) "\ngit push -u origin main" }
        }
    }
}

pub fn not_found() -> Markup {
    page("not found", None, html! { div.card { h2 { "404" } p.muted { "Nothing here." } } })
}

pub fn error(msg: &str) -> Markup {
    page("error", None, html! { div.card { h2 { "Something broke" } pre { (msg) } } })
}

fn flash(err: Option<&str>) -> Markup {
    html! { @if let Some(e) = err { div.flash { (e) } } }
}

pub fn login(next: &str, err: Option<&str>) -> Markup {
    html! {
        div.narrow {
            h1.page-title { "sign in" }
            (flash(err))
            form.card.form method="post" action="/login" {
                input type="hidden" name="next" value=(next);
                label { "user" input name="name" autocomplete="username" required autofocus; }
                label { "password" input type="password" name="password" autocomplete="current-password" required; }
                button.btn.primary { "sign in" }
            }
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum SettingsTab {
    Keys,
    Tokens,
    Password,
}

fn settings_nav(tab: SettingsTab) -> Markup {
    html! {
        nav.tabs {
            a.active[tab == SettingsTab::Keys] href="/settings/keys" { "ssh keys" }
            a.active[tab == SettingsTab::Tokens] href="/settings/tokens" { "access tokens" }
            a.active[tab == SettingsTab::Password] href="/settings/password" { "password" }
        }
    }
}

pub fn keys(keys: &[SshKey], err: Option<&str>) -> Markup {
    html! {
        h1.page-title { "settings" }
        (settings_nav(SettingsTab::Keys))
        div.stack.mt {
            (flash(err))
            div.card.flush {
                div.card-head { "ssh keys" span.muted { (keys.len()) } }
                @if keys.is_empty() { p.pad.muted { "No keys yet." } }
                ul.rows {
                    @for k in keys {
                        li {
                            div { strong { (k.title) } div.muted.mono { (k.fingerprint) } }
                            span.grow {}
                            span.muted { "added " (ago(k.created)) }
                            form.inline method="post" action={ "/settings/keys/" (k.id) "/delete" } { button.link.danger { "delete" } }
                        }
                    }
                }
            }
            form.card.form method="post" action="/settings/keys" {
                label { "title" input name="title" placeholder="laptop"; }
                label { "public key" textarea name="key" rows="3" placeholder="ssh-ed25519 AAAA..." required {} }
                button.btn.primary { "add key" }
            }
        }
    }
}

pub fn tokens(tokens: &[Token], new_token: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        h1.page-title { "settings" }
        (settings_nav(SettingsTab::Tokens))
        div.stack.mt {
            (flash(err))
            @if let Some(t) = new_token {
                div.card.notice {
                    p { "New token. Copy it now. It won't be shown again." }
                    input readonly value=(t);
                    p.muted { "Use it as the password for HTTPS git: " code { "git clone https://<user>:<token>@host/owner/repo.git" } }
                }
            }
            div.card.flush {
                div.card-head { "access tokens" span.muted { (tokens.len()) } }
                @if tokens.is_empty() { p.pad.muted { "No tokens yet." } }
                ul.rows {
                    @for t in tokens {
                        li {
                            strong { (t.title) }
                            span.grow {}
                            span.muted {
                                "created " (ago(t.created))
                                @if let Some(u) = t.last_used { " · used " (ago(u)) } @else { " · never used" }
                            }
                            form.inline method="post" action={ "/settings/tokens/" (t.id) "/delete" } { button.link.danger { "revoke" } }
                        }
                    }
                }
            }
            form.card.form method="post" action="/settings/tokens" {
                label { "title" input name="title" placeholder="ci, laptop..." required; }
                button.btn.primary { "create token" }
            }
        }
    }
}

pub fn password(msg: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        h1.page-title { "settings" }
        (settings_nav(SettingsTab::Password))
        div.stack.mt.narrow-left {
            (flash(err))
            @if let Some(m) = msg { div.card.notice { (m) } }
            form.card.form method="post" action="/settings/password" {
                label { "current password" input type="password" name="current" autocomplete="current-password" required; }
                label { "new password" input type="password" name="new" autocomplete="new-password" minlength="8" required; }
                button.btn.primary { "change password" }
            }
        }
    }
}

pub fn new_repo(owner: &str, err: Option<&str>) -> Markup {
    html! {
        div.narrow {
            h1.page-title { "new repository" }
            (flash(err))
            form.card.form method="post" action="/new" {
                label { "name" div.prefixed { span.muted { (owner) " /" } input name="name" required autofocus pattern="[A-Za-z0-9._-]+"; } }
                label { "description" input name="description"; }
                label.check { input type="checkbox" name="private" value="1"; "private" }
                button.btn.primary { "create" }
            }
        }
    }
}
