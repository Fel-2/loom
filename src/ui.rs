//! HTML views. Server-rendered, no JS required.

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::db::{Access, Collaborator, Member, Owner, Protection, Repo, SshKey, Token, User};
use crate::git::{CommitInfo, Entry, FileDiff, Line, RefInfo};

pub fn page(title: &str, viewer: Option<&User>, body: Markup) -> Markup {
    page_head(title, viewer, body, html! {})
}

/// `page` with extra `<head>` content (feed links, OpenGraph tags).
pub fn page_head(title: &str, viewer: Option<&User>, body: Markup, head_extra: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · loom" }
                link rel="stylesheet" href="/static/style.css";
                (head_extra)
            }
            body {
                header.top {
                    a.brand href="/" { "loom" }
                    form.search-box method="get" action="/search" { input type="search" name="q" placeholder="search" aria-label="search"; }
                    span.grow {}
                    @if let Some(u) = viewer {
                        a.bell href="/notifications" title="notifications" { "🔔" @if u.unread > 0 { span.unread { (u.unread) } } }
                        a.btn href="/new" { "+ new" }
                        a href={ "/" (u.name) } { (avatar(&u.name, 18)) } a href="/settings/keys" { (u.name) }
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

pub fn flash_box(err: Option<&str>, msg: Option<&str>) -> Markup {
    html! {
        @if let Some(e) = err { div.flash { (e) } }
        @if let Some(m) = msg { div.card.notice { (m) } }
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
    Issues,
    Pulls,
    Releases,
    Settings,
}

/// Social/permission bits shown in the repo header.
pub struct Meta {
    pub stars: i64,
    /// `None` when signed out.
    pub starred: Option<bool>,
    pub forks: i64,
    pub parent: Option<String>,
    pub can_admin: bool,
    pub open_issues: i64,
    pub open_pulls: i64,
    /// `None` when signed out.
    pub watching: Option<bool>,
}

pub struct Clone<'a> {
    pub http: &'a str,
    pub ssh: Option<&'a str>,
}

pub fn repo_header(repo: &Repo, clone: &Clone<'_>, meta: &Meta, tab: Tab, rev: &str) -> Markup {
    let (owner, name) = (&repo.owner, &repo.name);
    let base = format!("/{owner}/{name}");
    html! {
        section.repo-head {
            div.repo-top {
                div {
                    div.repo-title {
                        a href={ "/" (owner) } { (owner) } span.sep { "/" } a href=(base) { strong { (name) } }
                        @if repo.private { span.badge { "private" } }
                        @if repo.archived { span.badge.warn { "archived" } }
                    }
                    @if let Some(p) = &meta.parent { p.muted.small { "forked from " a href={ "/" (p) } { (p) } } }
                    @if !repo.description.is_empty() { p.muted { (repo.description) } }
                }
                div.actions {
                    @if let Some(w) = meta.watching {
                        form.inline method="post" action={ (base) "/watch" } {
                            button.btn.pill title=@if w { "you get notified about all activity" } @else { "get notified about all activity" } { @if w { "◉ watching" } @else { "○ watch" } }
                        }
                    }
                    @match meta.starred {
                        Some(on) => form.inline method="post" action={ (base) "/star" } {
                            button.btn.pill { @if on { "★ starred" } @else { "☆ star" } span.count { (meta.stars) } }
                        },
                        None => span.btn.pill.static { "☆ " span.count { (meta.stars) } },
                    }
                    @if meta.starred.is_some() {
                        a.btn.pill href={ (base) "/fork" } { "⑂ fork" span.count { (meta.forks) } }
                    } @else {
                        span.btn.pill.static { "⑂ " span.count { (meta.forks) } }
                    }
                    details.clone {
                        summary.btn.pill { "clone" }
                        div.card {
                            label { "https" } input readonly value=(clone.http);
                            @if let Some(s) = clone.ssh { label { "ssh" } input readonly value=(s); }
                        }
                    }
                }
            }
            nav.tabs {
                a.active[tab == Tab::Overview] href=(base) { "overview" }
                a.active[tab == Tab::Commits] href={ (base) "/log/" (rev) } { "commits" }
                a.active[tab == Tab::Refs] href={ (base) "/refs" } { "branches & tags" }
                a.active[tab == Tab::Issues] href={ (base) "/issues" } { "issues" @if meta.open_issues > 0 { span.tab-count { (meta.open_issues) } } }
                a.active[tab == Tab::Pulls] href={ (base) "/pulls" } { "pulls" @if meta.open_pulls > 0 { span.tab-count { (meta.open_pulls) } } }
                a.active[tab == Tab::Releases] href={ (base) "/releases" } { "releases" }
                a href={ (base) "/search" } { "search" }
                @if meta.can_admin { a.active[tab == Tab::Settings] href={ (base) "/settings" } { "settings" } }
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

pub struct LastCommit {
    pub sha: String,
    pub author: String,
    pub when: i64,
}

pub fn tree(base: &str, rev: &str, path: &str, entries: &[Entry], last: &std::collections::HashMap<String, LastCommit>) -> Markup {
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
                            a.dir href={ (base) "/tree/" (rev) "/" (prefix) (e.name) } { span.icon { "▸" } (e.name) "/" @if let Some(l) = last.get(&e.name) { span.last-commit { (l.sha) " · " (l.author) " · " (ago(l.when)) } } }
                        } @else {
                            a href={ (base) "/blob/" (rev) "/" (prefix) (e.name) } { span.icon { "·" } (e.name) @if let Some(l) = last.get(&e.name) { span.last-commit { (l.sha) " · " (l.author) " · " (ago(l.when)) } } }
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
    markdown_refs(src, &NoRefs)
}

/// Resolves reference tokens to links. Return `None` to leave the text as is
/// (unknown target, or the viewer can't see it).
pub trait Resolve {
    fn link(&self, t: &crate::refs::Token) -> Option<RefLink>;
}

pub struct RefLink {
    pub href: String,
    pub title: String,
    pub class: &'static str,
}

struct NoRefs;
impl Resolve for NoRefs {
    fn link(&self, _: &crate::refs::Token) -> Option<RefLink> {
        None
    }
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn url_scheme_ok(url: &str) -> bool {
    let cleaned: String = url.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect();
    if cleaned.starts_with(['/', '#', '?', '.']) {
        return true;
    }
    match cleaned.split_once(':') {
        None => true,
        Some((scheme, _)) => matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "mailto" | "ftp"),
    }
}

fn safe_dest(url: pulldown_cmark::CowStr<'_>) -> pulldown_cmark::CowStr<'_> {
    if url_scheme_ok(&url) { url } else { pulldown_cmark::CowStr::from("#") }
}

/// Markdown with raw HTML neutralized and references linked (outside code
/// and existing links).
pub fn markdown_refs(src: &str, r: &dyn Resolve) -> Markup {
    use crate::refs::{scan, Segment};
    use pulldown_cmark::{html::push_html, CowStr, Event, Options, Parser, Tag, TagEnd};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut depth = 0usize;
    let mut events: Vec<Event<'_>> = Vec::new();
    for e in Parser::new_ext(src, opts) {
        match e {
            // Raw HTML in user content is rendered as text: no sanitizer, no XSS.
            Event::Html(h) | Event::InlineHtml(h) => events.push(Event::Text(h)),
            Event::Start(t @ (Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. })) => {
                depth += 1;
                events.push(Event::Start(match t {
                    Tag::Link { link_type, dest_url, title, id } => {
                        Tag::Link { link_type, dest_url: safe_dest(dest_url), title, id }
                    }
                    Tag::Image { link_type, dest_url, title, id } => {
                        Tag::Image { link_type, dest_url: safe_dest(dest_url), title, id }
                    }
                    other => other,
                }));
            }
            Event::End(t @ (TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image)) => {
                depth = depth.saturating_sub(1);
                events.push(Event::End(t));
            }
            Event::Text(t) if depth == 0 => {
                for seg in scan(&t) {
                    match seg {
                        Segment::Ref(tok, raw) => match r.link(&tok) {
                            Some(l) => events.push(Event::InlineHtml(CowStr::from(format!(
                                "<a class=\"{}\" href=\"{}\" title=\"{}\">{}</a>",
                                l.class,
                                escape(&l.href),
                                escape(&l.title),
                                escape(raw)
                            )))),
                            None => events.push(Event::Text(CowStr::from(raw.to_owned()))),
                        },
                        Segment::Text(x) => events.push(Event::Text(CowStr::from(x.to_owned()))),
                    }
                }
            }
            e => events.push(e),
        }
    }
    let mut out = String::with_capacity(src.len() * 3 / 2);
    push_html(&mut out, events.into_iter());
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

pub const MAX_VIEW_LINES: usize = 5000;
pub const MAX_DIFF_LINES: usize = 4000;
pub const MAX_DIFF_FILES: usize = 200;

pub fn blob(base: &str, rev: &str, path: &str, data: &[u8]) -> Markup {
    let binary = data.iter().take(8000).any(|&b| b == 0);
    let text = String::from_utf8_lossy(data);
    let all: Vec<&str> = text.lines().collect();
    let truncated = all.len() > MAX_VIEW_LINES;
    let shown = &all[..all.len().min(MAX_VIEW_LINES)];
    // Big files aren't read line by line; skip the per-line HTML for them
    // (bounds CPU and peak memory under load).
    let hl = (!binary && data.len() <= 128 * 1024).then(|| crate::highlight::lines(path, &shown.join("\n")));
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
                    @for (i, l) in shown.iter().enumerate() {
                        tr id={ "L" (i + 1) } {
                            td.ln { a href={ "#L" (i + 1) } { (i + 1) } }
                            @match &hl {
                                Some(h) => td.src { (PreEscaped(h.get(i).cloned().unwrap_or_default())) },
                                None => td.src { (l) },
                            }
                        }
                    }
                }
                @if truncated {
                    p.pad.muted { "Showing the first " (MAX_VIEW_LINES) " of " (all.len()) " lines. " a href={ (base) "/raw/" (rev) "/" (path) } { "Download the file" } " for the rest." }
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
        @if files.len() > MAX_DIFF_FILES { p.muted.small { "Showing the first " (MAX_DIFF_FILES) " of " (files.len()) " changed files." } }
        nav.card.filelist {
            @for (i, f) in files.iter().take(MAX_DIFF_FILES).enumerate() {
                a href={ "#f" (i) } {
                    span class={ "st st-" (f.status) } { (f.status) } (f.path)
                    span.grow {} span.add { "+" (f.adds) } span.del { "-" (f.dels) }
                }
            }
        }
        @for (i, f) in files.iter().take(MAX_DIFF_FILES).enumerate() {
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
                        @for l in f.lines.iter().take(MAX_DIFF_LINES) {
                            @match l {
                                Line::Hunk(h) => tr.hunk { td.ln {} td.ln {} td.src { (h) } },
                                Line::Ctx(a, b, t) => tr { td.ln { (a) } td.ln { (b) } td.src { " " (t) } },
                                Line::Del(a, t) => tr.del { td.ln { (a) } td.ln {} td.src { "-" (t) } },
                                Line::Add(b, t) => tr.add { td.ln {} td.ln { (b) } td.src { "+" (t) } },
                            }
                        }
                    }
                    @if f.lines.len() > MAX_DIFF_LINES { p.pad.muted.small { "Diff truncated at " (MAX_DIFF_LINES) " lines; see the commit or clone for the rest." } }
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
    TwoFactor,
    Account,
}

fn settings_nav(tab: SettingsTab) -> Markup {
    html! {
        nav.tabs {
            a.active[tab == SettingsTab::Keys] href="/settings/keys" { "ssh keys" }
            a.active[tab == SettingsTab::Tokens] href="/settings/tokens" { "access tokens" }
            a.active[tab == SettingsTab::Password] href="/settings/password" { "password" }
            a.active[tab == SettingsTab::TwoFactor] href="/settings/2fa" { "two-factor" }
            a.active[tab == SettingsTab::Account] href="/settings/account" { "account" }
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

fn owner_select(owners: &[String], selected: &str) -> Markup {
    html! {
        @if owners.len() > 1 {
            select name="owner" {
                @for o in owners { option value=(o) selected[o == selected] { (o) } }
            }
        } @else {
            span.muted { (selected) } input type="hidden" name="owner" value=(selected);
        }
    }
}

pub fn new_repo(owners: &[String], selected: &str, err: Option<&str>) -> Markup {
    html! {
        div.narrow {
            h1.page-title { "new repository" }
            (flash(err))
            form.card.form method="post" action="/new" {
                label { "name" div.prefixed { (owner_select(owners, selected)) span.muted { "/" } input name="name" required autofocus pattern="[A-Za-z0-9._-]+"; } }
                label { "description" input name="description"; }
                label.check { input type="checkbox" name="private" value="1"; "private" }
                button.btn.primary { "create" }
            }
            p.muted.small { "Or " a href="/new/org" { "create an organization" } "." }
        }
    }
}

pub fn new_org(err: Option<&str>) -> Markup {
    html! {
        div.narrow {
            h1.page-title { "new organization" }
            (flash(err))
            form.card.form method="post" action="/new/org" {
                label { "name" input name="name" required autofocus pattern="[A-Za-z0-9._-]+"; }
                label { "description" input name="description"; }
                button.btn.primary { "create organization" }
            }
        }
    }
}

pub fn fork(repo: &Repo, owners: &[String], err: Option<&str>) -> Markup {
    let base = format!("/{}/{}", repo.owner, repo.name);
    html! {
        div.narrow {
            h1.page-title { "fork " (repo.owner) "/" (repo.name) }
            (flash(err))
            form.card.form method="post" action={ (base) "/fork" } {
                label { "into" div.prefixed { (owner_select(owners, &owners[0])) span.muted { "/" } input name="name" value=(repo.name) required pattern="[A-Za-z0-9._-]+"; } }
                @if repo.private { p.muted.small { "The fork will be private, like its parent." } }
                button.btn.primary { "fork" }
            }
        }
    }
}

fn role_select(name: &str, current: Access) -> Markup {
    html! {
        select name=(name) {
            option value="read" selected[current == Access::Read] { "read" }
            option value="write" selected[current == Access::Write] { "write" }
            option value="admin" selected[current == Access::Admin] { "admin" }
        }
    }
}

fn role_name(a: Access) -> &'static str {
    match a {
        Access::Admin => "admin",
        Access::Write => "write",
        Access::Read => "read",
        Access::None => "none",
    }
}

pub struct TeamRow {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub members: usize,
    pub repos: usize,
}

pub struct SettingsData<'a> {
    pub branches: &'a [String],
    pub default_branch: Option<&'a str>,
    pub collaborators: &'a [Collaborator],
    pub protections: &'a [Protection],
    pub owners: &'a [String],
    pub teams: &'a [(String, Access)],
}

pub fn repo_settings(repo: &Repo, d: &SettingsData<'_>, msg: Option<&str>, err: Option<&str>) -> Markup {
    let base = format!("/{}/{}/settings", repo.owner, repo.name);
    html! {
        div.stack {
            (flash(err))
            @if let Some(m) = msg { div.card.notice { (m) } }

            form.card.form method="post" action={ (base) "/general" } {
                h3 { "general" }
                label { "description" input name="description" value=(repo.description); }
                @if !d.branches.is_empty() {
                    label { "default branch"
                        select name="default_branch" {
                            @for b in d.branches { option value=(b) selected[Some(b.as_str()) == d.default_branch] { (b) } }
                        }
                    }
                }
                label.check { input type="checkbox" name="private" value="1" checked[repo.private]; "private" }
                label.check { input type="checkbox" name="archived" value="1" checked[repo.archived]; "archived (read-only, pushes refused)" }
                label.check { input type="checkbox" name="show_last_commit" value="1" checked[repo.show_last_commit]; "show last commit per file in tree views (slower on large directories)" }
                button.btn.primary { "save" }
            }

            section.card.flush {
                div.card-head { "collaborators" span.muted { (d.collaborators.len()) } }
                @if d.collaborators.is_empty() { p.pad.muted { "Only the owner and admins have access." } }
                ul.rows {
                    @for c in d.collaborators {
                        li {
                            a href={ "/" (c.name) } { strong { (c.name) } }
                            span.badge { (role_name(c.access)) }
                            span.grow {}
                            form.inline method="post" action={ (base) "/collaborators/remove" } {
                                input type="hidden" name="name" value=(c.name);
                                button.link.danger { "remove" }
                            }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/collaborators" } {
                    input name="name" placeholder="user name" required;
                    (role_select("role", Access::Write))
                    button.btn { "add / update" }
                }
            }

            section.card.flush {
                div.card-head { "branch protection" span.muted { (d.protections.len()) } }
                @if d.protections.is_empty() { p.pad.muted { "No protected branches." } }
                ul.rows {
                    @for p in d.protections {
                        li {
                            code { (p.branch) }
                            span.muted.small {
                                @if p.allow_force_push { "force-push allowed" } @else { "no force-push" }
                                " · "
                                @if p.allow_delete { "deletion allowed" } @else { "no deletion" }
                                @if p.require_pr { " · PRs only" }
                                @if !p.required_checks.is_empty() { " · checks: " (p.required_checks.join(", ")) }
                            }
                            span.grow {}
                            form.inline method="post" action={ (base) "/protect/remove" } {
                                input type="hidden" name="branch" value=(p.branch);
                                button.link.danger { "unprotect" }
                            }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/protect" } {
                    input name="branch" placeholder="branch, e.g. main" required value=[d.default_branch];
                    label.check { input type="checkbox" name="allow_force_push" value="1"; "allow force-push" }
                    label.check { input type="checkbox" name="allow_delete" value="1"; "allow delete" }
                    label.check { input type="checkbox" name="require_pr" value="1"; "require pull request" }
                    input name="required_checks" placeholder="required checks, e.g. ci/jenkins";
                    button.btn { "protect" }
                }
            }

            section.card.flush {
                div.card-head { "teams with access" span.muted { (d.teams.len()) } }
                @if d.teams.is_empty() { p.pad.muted { "No teams. Manage teams in the organization's settings." } }
                ul.rows { @for (t, role) in d.teams { li { strong { (t) } span.badge { (role_name(*role)) } } } }
            }

            section.card.danger-zone {
                h3 { "danger zone" }
                form.row-form method="post" action={ (base) "/rename" } {
                    span { "rename" }
                    input name="name" value=(repo.name) required pattern="[A-Za-z0-9._-]+";
                    button.btn { "rename" }
                }
                form.row-form method="post" action={ (base) "/transfer" } {
                    span { "transfer to" }
                    select name="owner" { @for o in d.owners { option value=(o) selected[*o == repo.owner] { (o) } } }
                    button.btn { "transfer" }
                }
                form.row-form method="post" action={ (base) "/delete" } {
                    span { "delete — type " code { (repo.owner) "/" (repo.name) } " to confirm" }
                    input name="confirm" required autocomplete="off";
                    button.btn.danger { "delete repository" }
                }
            }
        }
    }
}

pub fn owner_page(owner: &Owner, repos: &[Repo], orgs: &[String], members: &[Member], can_admin: bool) -> Markup {
    html! {
        div.owner-head {
            h1.page-title { (owner.name) @if owner.is_org { span.badge { "org" } } }
            @if !owner.description.is_empty() { p.muted { (owner.description) } }
            @if can_admin && owner.is_org { a.btn href={ "/" (owner.name) "/settings" } { "org settings" } }
        }
        div.split {
            div { (index(repos, None)) }
            aside.stack {
                @if !orgs.is_empty() {
                    div.card.flush {
                        div.card-head { "organizations" }
                        ul.rows { @for o in orgs { li { a href={ "/" (o) } { (o) } } } }
                    }
                }
                @if owner.is_org {
                    div.card.flush {
                        div.card-head { "members" span.muted { (members.len()) } }
                        ul.rows { @for m in members { li { a href={ "/" (m.name) } { (m.name) } span.grow {} span.badge { (role_name(m.access)) } } } }
                    }
                }
            }
        }
    }
}

pub fn org_settings(org: &Owner, members: &[Member], repo_count: i64, teams: &[TeamRow], msg: Option<&str>, err: Option<&str>) -> Markup {
    let base = format!("/{}/settings", org.name);
    html! {
        h1.page-title { a href={ "/" (org.name) } { (org.name) } " · settings" }
        div.stack.narrow-left {
            (flash(err))
            @if let Some(m) = msg { div.card.notice { (m) } }
            form.card.form method="post" action={ (base) "/general" } {
                label { "description" input name="description" value=(org.description); }
                button.btn.primary { "save" }
            }
            section.card.flush {
                div.card-head { "members" span.muted { (members.len()) } }
                ul.rows {
                    @for m in members {
                        li {
                            a href={ "/" (m.name) } { strong { (m.name) } }
                            span.badge { (role_name(m.access)) }
                            span.grow {}
                            form.inline method="post" action={ (base) "/members/remove" } {
                                input type="hidden" name="name" value=(m.name);
                                button.link.danger { "remove" }
                            }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/members" } {
                    input name="name" placeholder="user name" required;
                    (role_select("role", Access::Write))
                    button.btn { "add / update" }
                }
                p.pad.muted.small { "read: see private repos · write: push and create repos · admin: manage org and all its repos" }
            }
            section.card.flush {
                div.card-head { "teams" span.muted { (teams.len()) } }
                @if teams.is_empty() { p.pad.muted { "No teams yet. Teams get read/write/admin on selected repositories." } }
                ul.rows {
                    @for t in teams {
                        li {
                            a href={ (base) "/teams/" (t.id) } { strong { (t.name) } }
                            span.muted.small { (t.description) }
                            span.grow {}
                            span.muted.small { (t.members) " members · " (t.repos) " repos" }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/teams" } {
                    input name="name" placeholder="team name" required maxlength="50" pattern="[A-Za-z0-9 ._-]+";
                    input name="description" placeholder="description" maxlength="200";
                    button.btn { "create team" }
                }
            }

            section.card.danger-zone {
                h3 { "danger zone" }
                @if repo_count > 0 {
                    p.muted { "Delete or transfer the org's " (repo_count) " repositories before deleting it." }
                } @else {
                    form.row-form method="post" action={ (base) "/delete" } {
                        span { "delete — type " code { (org.name) } " to confirm" }
                        input name="confirm" required autocomplete="off";
                        button.btn.danger { "delete organization" }
                    }
                }
            }
        }
    }
}

pub fn totp_login(user: &str, err: Option<&str>) -> Markup {
    html! {
        div.narrow {
            h1.page-title { "two-factor" }
            p.muted { "Signed in as " strong { (user) } ". Enter the 6-digit code from your authenticator app." }
            (flash(err))
            form.card.form method="post" action="/login/totp" {
                label { "code" input name="code" inputmode="numeric" pattern="[0-9 ]{6,7}" maxlength="7" autocomplete="one-time-code" required autofocus; }
                button.btn.primary { "verify" }
            }
        }
    }
}

pub fn totp_settings(user: &str, secret: Option<&str>, enabled: bool, msg: Option<&str>) -> Markup {
    html! {
        h1.page-title { "settings" }
        (settings_nav(SettingsTab::TwoFactor))
        div.stack.mt.narrow-left {
            @if let Some(m) = msg { div.card.notice { (m) } }
            @if enabled {
                div.card {
                    p { strong { "Two-factor is on." } " Logins require a code from your authenticator app." }
                }
                form.card.form method="post" action="/settings/2fa/disable" {
                    label { "code (to turn it off)" input name="code" inputmode="numeric" maxlength="7" required; }
                    button.btn.danger { "turn off two-factor" }
                }
            } @else if let Some(s) = secret {
                div.card {
                    p { "1. In your authenticator app, add an account and enter this secret:" }
                    pre { (s) }
                    p.muted.small { "or open " code { (crate::totp::otpauth_url(user, s, "loom")) } }
                    p { "2. Enter the 6-digit code it shows:" }
                }
                form.card.form method="post" action="/settings/2fa/enable" {
                    input type="hidden" name="secret" value=(s);
                    label { "code" input name="code" inputmode="numeric" maxlength="7" required autofocus; }
                    button.btn.primary { "turn on two-factor" }
                }
            } @else {
                p.muted { "Two-factor is off." }
            }
        }
    }
}

pub fn blame(base: &str, rev: &str, path: &str, lines: &[crate::git::BlameLine], total: usize) -> Markup {
    html! {
        div.card.flush {
            div.card-head {
                span { (path) }
                span.muted { (total) " lines" }
                span.grow {}
                a href={ (base) "/blob/" (rev) "/" (path) } { "view file" }
            }
            table.code.blame {
                @for (i, l) in lines.iter().enumerate() {
                    @let prev = i.checked_sub(1).and_then(|p| lines.get(p)).map(|p| p.sha.as_str());
                    tr {
                        @if prev != Some(l.sha.as_str()) {
                            td.gutter title={ (l.author) " · " (ago(l.time)) } {
                                a.sha href={ (base) "/commit/" (l.sha) } { (&l.sha[..8]) }
                                span.author { (l.author) }
                            }
                        } @else { td.gutter.cont {} }
                        td.ln { (i + 1) }
                        td.src { (l.text) }
                    }
                }
            }
            @if total > lines.len() { p.pad.muted { "Blame shown for the first " (lines.len()) " of " (total) " lines." } }
        }
    }
}

/// Deterministic identicon from a name (no external avatar service).
pub fn avatar(name: &str, size: u32) -> Markup {
    use sha2::Digest;
    let h = sha2::Sha256::digest(name.as_bytes());
    let hue = h[0] as u32 * 360 / 256;
    let bg = format!("hsl({hue} 55% 45%)");
    let fg = format!("hsl({hue} 65% 85%)");
    let mut cells = String::new();
    for y in 0..5 {
        for x in 0..3 {
            let on = h[1 + y * 3 + x] & 1 == 1;
            if on {
                for xx in [x, 4 - x] {
                    cells.push_str(&format!("<rect x=\"{xx}\" y=\"{y}\" width=\"1\" height=\"1\"/>"));
                }
            }
        }
    }
    PreEscaped(format!(
        "<svg class=\"avatar\" width=\"{size}\" height=\"{size}\" viewBox=\"0 0 5 5\" aria-hidden=\"true\">         <rect width=\"5\" height=\"5\" fill=\"{bg}\"/><g fill=\"{fg}\">{cells}</g></svg>"
    ))
}

/// Avatar + name link.
pub fn user_link(name: &str, size: u32) -> Markup {
    html! { a.who href={ "/" (name) } { (avatar(name, size)) (name) } }
}

pub fn account(email: &str, enabled: bool, msg: Option<&str>, err: Option<&str>) -> Markup {
    html! {
        h1.page-title { "settings" }
        (settings_nav(SettingsTab::Account))
        div.stack.mt.narrow-left {
            (flash_box(err, msg))
            form.card.form method="post" action="/settings/account" {
                label { "email" input name="email" type="email" value=(email) maxlength="200" placeholder="you@example.com"; }
                label.check { input type="checkbox" name="notify" value="1" checked[enabled]; "email me about mentions, assignments and my threads" }
                p.muted.small { "Sending needs LOOM_MAIL_CMD on the server (e.g. " code { "/usr/sbin/sendmail -t" } ")." }
                button.btn.primary { "save" }
            }
        }
    }
}

pub fn team_page(org: &Owner, team: &TeamRow, members: &[Member], repos: &[(String, Access)], all_repos: &[String], msg: Option<&str>, err: Option<&str>) -> Markup {
    let base = format!("/{}/settings/teams/{}", org.name, team.id);
    html! {
        h1.page-title { a href={ "/" (org.name) "/settings" } { (org.name) } " · " (team.name) }
        div.stack.narrow-left {
            (flash_box(err, msg))
            section.card.flush {
                div.card-head { "members" span.muted { (members.len()) } }
                ul.rows {
                    @for m in members {
                        li {
                            a href={ "/" (m.name) } { (m.name) }
                            span.badge { (role_name(m.access)) }
                            span.grow {}
                            form.inline method="post" action={ (base) "/members/remove" } {
                                input type="hidden" name="name" value=(m.name);
                                button.link.danger { "remove" }
                            }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/members" } {
                    input name="name" placeholder="user name" required;
                    select name="role" { option value="read" { "read" } option value="write" selected { "write" } option value="admin" { "admin" } }
                    button.btn { "add / update" }
                }
            }
            section.card.flush {
                div.card-head { "repository access" span.muted { (repos.len()) } }
                @if repos.is_empty() { p.pad.muted { "No repositories granted." } }
                ul.rows {
                    @for (r, role) in repos {
                        li {
                            a href={ "/" (r) } { (r) }
                            span.badge { (role_name(*role)) }
                            span.grow {}
                            form.inline method="post" action={ (base) "/repos/remove" } {
                                input type="hidden" name="repo" value=(r);
                                button.link.danger { "revoke" }
                            }
                        }
                    }
                }
                form.row-form method="post" action={ (base) "/repos" } {
                    select name="repo" { @for r in all_repos { option value=(r) { (r) } } }
                    select name="role" { option value="read" { "read" } option value="write" selected { "write" } option value="admin" { "admin" } }
                    button.btn { "grant" }
                }
            }
            section.card.danger-zone {
                h3 { "danger zone" }
                form.row-form method="post" action={ (base) "/delete" } {
                    span { "delete team — type " code { (team.name) } " to confirm" }
                    input name="confirm" required autocomplete="off";
                    button.btn.danger { "delete team" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_urls() {
        for ok in ["https://a/b", "http://a", "mailto:a@b", "ftp://h/f", "/x/y", "./x", "#frag", "?q=1"] {
            assert!(url_scheme_ok(ok), "unexpectedly blocked: {ok}");
        }
        for bad in ["javascript:alert(1)", " JaVaScRiPt:alert(1)", "java\tscript:alert(1)", "data:text/html,x", "vbscript:x", "file:///etc/passwd", "a:b"] {
            assert!(!url_scheme_ok(bad), "unexpectedly allowed: {bad}");
        }
    }

    #[test]
    fn markdown_drops_unsafe_links() {
        let out = markdown_refs("[a](javascript:alert(1)) [b](https://ok.test/)", &NoRefs).into_string();
        assert!(!out.contains("javascript:"), "{out}");
        assert!(out.contains("https://ok.test/"), "{out}");
    }
}
