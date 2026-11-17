//! Releases: tagged versions with notes and uploaded assets, plus source
//! archives (`git archive`). Storage: data/releases/<repo id>/<release id>/.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path as AxPath, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use maud::{html, Markup};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::app::App;
use crate::db::{now, Access, Db, Repo, Result as DbResult, User};
use crate::ui::{ago, flash_box, Tab};
use crate::web::{blocking, see_other, Authed, Ctx, Err, Res, St, Viewer};

const MAX_ASSET: u64 = 200 * 1024 * 1024;
const MAX_BODY: usize = 64 * 1024;

pub struct Release {
    pub id: i64,
    pub tag: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created: i64,
    pub draft: bool,
    pub prerelease: bool,
}

pub struct Asset {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub downloads: i64,
}

impl Db {
    pub fn releases(&self, repo_id: i64) -> DbResult<Vec<Release>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT r.id, r.tag, r.title, r.body, u.name, r.created, r.draft, r.prerelease FROM releases r
             LEFT JOIN users u ON u.id = r.author_id WHERE r.repo_id = ?1 ORDER BY r.created DESC, r.id DESC",
        )?;
        st.query_map([repo_id], |r| {
            Ok(Release {
                id: r.get(0)?,
                tag: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
                author: r.get(4)?,
                created: r.get(5)?,
                draft: r.get(6)?,
                prerelease: r.get(7)?,
            })
        })?
        .collect()
    }

    pub fn release(&self, repo_id: i64, id: i64) -> DbResult<Option<Release>> {
        self.c()
            .query_row(
                "SELECT r.id, r.tag, r.title, r.body, u.name, r.created, r.draft, r.prerelease FROM releases r
                 LEFT JOIN users u ON u.id = r.author_id WHERE r.repo_id = ?1 AND r.id = ?2",
                params![repo_id, id],
                |r| {
                    Ok(Release {
                        id: r.get(0)?,
                        tag: r.get(1)?,
                        title: r.get(2)?,
                        body: r.get(3)?,
                        author: r.get(4)?,
                        created: r.get(5)?,
                        draft: r.get(6)?,
                        prerelease: r.get(7)?,
                    })
                },
            )
            .optional()
    }

    pub fn create_release(&self, repo_id: i64, tag: &str, title: &str, body: &str, author: i64, draft: bool, prerelease: bool) -> DbResult<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO releases (repo_id, tag, title, body, author_id, created, draft, prerelease)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![repo_id, tag, title, body, author, now(), draft, prerelease],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_release(&self, repo_id: i64, id: i64, title: &str, body: &str, draft: bool, prerelease: bool) -> DbResult<()> {
        self.c().execute(
            "UPDATE releases SET title = ?1, body = ?2, draft = ?3, prerelease = ?4 WHERE id = ?5 AND repo_id = ?6",
            params![title, body, draft, prerelease, id, repo_id],
        )?;
        Ok(())
    }

    pub fn delete_release(&self, repo_id: i64, id: i64) -> DbResult<()> {
        self.c().execute("DELETE FROM releases WHERE id = ?1 AND repo_id = ?2", params![id, repo_id])?;
        Ok(())
    }

    pub fn release_assets(&self, release_id: i64) -> DbResult<Vec<Asset>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, name, size, downloads FROM release_assets WHERE release_id = ?1 ORDER BY name")?;
        st.query_map([release_id], |r| {
            Ok(Asset { id: r.get(0)?, name: r.get(1)?, size: r.get(2)?, downloads: r.get(3)? })
        })?
        .collect()
    }

    pub fn add_asset(&self, release_id: i64, name: &str, size: i64) -> DbResult<i64> {
        let c = self.c();
        c.execute("INSERT INTO release_assets (release_id, name, size, created) VALUES (?1, ?2, ?3, ?4)", params![release_id, name, size, now()])?;
        Ok(c.last_insert_rowid())
    }

    pub fn asset(&self, repo_id: i64, id: i64) -> DbResult<Option<(i64, String, i64)>> {
        self.c()
            .query_row(
                "SELECT a.release_id, a.name, a.size FROM release_assets a JOIN releases r ON r.id = a.release_id
                 WHERE a.id = ?1 AND r.repo_id = ?2",
                params![id, repo_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }


    pub fn count_download(&self, id: i64) -> DbResult<()> {
        self.c().execute("UPDATE release_assets SET downloads = downloads + 1 WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn asset_dir(app: &App, repo: &Repo, release_id: i64) -> PathBuf {
    app.data.join("releases").join(repo.id.to_string()).join(release_id.to_string())
}

/// Basename only, conservative characters.
pub fn safe_asset_name(s: &str) -> Option<String> {
    let base = s.rsplit(['/', '\\']).next()?.trim();
    (!base.is_empty() && base.len() <= 200 && !base.starts_with('.') && base.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+~".contains(&b)))
        .then(|| base.to_owned())
}

fn admin(app: &Arc<App>, u: User, owner: &str, name: &str) -> Result<Ctx, Err> {
    Ctx::load_with(app, Some(u), owner, name, Access::Write)
}

// ---------- views ----------

fn human_size(b: i64) -> String {
    let b = b as f64;
    if b >= 1e9 { format!("{:.1} GB", b / 1e9) } else if b >= 1e6 { format!("{:.1} MB", b / 1e6) } else if b >= 1e3 { format!("{:.0} KB", b / 1e3) } else { format!("{b:.0} B") }
}

pub fn list_page(ctx: &Ctx, releases: &[(Release, Vec<Asset>)], can_edit: bool) -> Markup {
    let base = ctx.base();
    html! {
        div.issue-bar {
            h2.page-title { "releases" }
            span.grow {}
            @if can_edit { a.btn.primary href={ (base) "/releases/new" } { "new release" } }
        }
        @if releases.is_empty() { div.card { p.muted { "No releases yet." } } }
        @for (r, assets) in releases {
            article.card.flush.release {
                div.card-head {
                    a href={ (base) "/releases/" (r.id) } { strong { (r.title) } }
                    code.ref-pill { (r.tag) }
                    @if r.draft { span.badge.warn { "draft" } }
                    @if r.prerelease { span.badge { "pre-release" } }
                    span.muted.small { @if let Some(a) = &r.author { (a) " · " } (ago(r.created)) }
                }
                @if !r.body.is_empty() { div.prose.pad { (crate::ui::markdown(&r.body)) } }
                div.pad.release-links {
                    a href={ (base) "/releases/" (r.id) } { "notes & downloads" }
                    " · "
                    a href={ (base) "/archive/" (r.tag) ".tar.gz" } { "source (tar.gz)" }
                    " · "
                    a href={ (base) "/tree/" (r.tag) } { "browse" }
                }
                @if !assets.is_empty() {
                    ul.assets { @for a in assets {
                        li { a href={ (base) "/releases/assets/" (a.id) } { (a.name) } span.muted { " " (human_size(a.size)) " · " (a.downloads) " downloads" } }
                    } }
                }
            }
        }
    }
}

pub fn new_page(ctx: &Ctx, tags: &[String], tag: &str, title: &str, body: &str, err: Option<&str>) -> Markup {
    let base = ctx.base();
    html! {
        div.stack {
            (flash_box(err, None))
            form.card.form method="post" action={ (base) "/releases/new" } {
                h3 { "new release" }
                label { "tag" input name="tag" value=(tag) required maxlength="200" list="tags" pattern="[A-Za-z0-9._/+\\-]+"; }
                @if !tags.is_empty() {
                    datalist id="tags" { @for t in tags.iter().take(50) { option value=(t) {} } }
                    p.muted.small { "Existing tags: " @for (i, t) in tags.iter().take(8).enumerate() { @if i > 0 { ", " } code { (t) } } }
                }
                label { "title" input name="title" value=(title) required maxlength="256"; }
                label { "notes (markdown)" textarea.body name="body" rows="10" { (body) } }
                label.check { input type="checkbox" name="draft" value="1"; "draft (hidden from everyone else)" }
                label.check { input type="checkbox" name="prerelease" value="1"; "pre-release" }
                button.btn.primary { "save release" }
            }
        }
    }
}

pub fn view_page(ctx: &Ctx, r: &Release, assets: &[Asset], can_edit: bool, msg: Option<&str>) -> Markup {
    let base = ctx.base();
    html! {
        div.stack {
            (flash_box(None, msg))
            article.card.flush {
                div.card-head {
                    strong { (r.title) }
                    code.ref-pill { (r.tag) }
                    span.muted.small { @if let Some(a) = &r.author { (a) " · " } (ago(r.created)) }
                    span.grow {}
                    @if can_edit {
                        form.inline method="post" action={ (base) "/releases/" (r.id) "/delete" } {
                            button.link.danger { "delete release" }
                        }
                    }
                }
                div.prose.pad { @if r.body.is_empty() { p.muted { "No notes." } } @else { (crate::ui::markdown(&r.body)) } }
                div.pad.release-links {
                    a href={ (base) "/archive/" (r.tag) ".tar.gz" } { "source code (tar.gz)" }
                }
            }
            section.card.flush {
                div.card-head { "assets" span.muted { (assets.len()) } }
                @if assets.is_empty() { p.pad.muted { "No files attached." } }
                ul.assets { @for a in assets {
                    li {
                        a href={ (base) "/releases/assets/" (a.id) } { (a.name) }
                        span.muted { " " (human_size(a.size)) " · " (a.downloads) " download" @if a.downloads != 1 { "s" } }
                        span.grow {}
                        @if can_edit {
                            form.inline method="post" action={ (base) "/releases/" (r.id) "/assets/" (a.id) "/delete" } { button.link.danger { "delete" } }
                        }
                    }
                } }
                @if can_edit {
                    form.row-form method="post" action={ (base) "/releases/" (r.id) "/assets" } enctype="multipart/form-data" {
                        input type="file" name="file" required;
                        button.btn { "upload" }
                    }
                }
            }
            @if can_edit {
                details.card.edit {
                    summary { "edit notes" }
                    form.form method="post" action={ (base) "/releases/" (r.id) "/edit" } {
                        input name="title" value=(r.title) required maxlength="256";
                        textarea.body name="body" rows="8" { (r.body) }
                        label.check { input type="checkbox" name="draft" value="1" checked[r.draft]; "draft" }
                        label.check { input type="checkbox" name="prerelease" value="1" checked[r.prerelease]; "pre-release" }
                        button.btn.primary { "save" }
                    }
                }
            }
        }
    }
}

// ---------- handlers ----------

async fn list(State(app): St, Viewer(v): Viewer, AxPath((owner, name)): AxPath<(String, String)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let can_edit = ctx.access >= Access::Write && !ctx.repo.archived;
    let mut all = Vec::new();
    for r in app.db.releases(ctx.repo.id)? {
        if r.draft && !can_edit {
            continue;
        }
        let assets = app.db.release_assets(r.id)?;
        all.push((r, assets));
    }
    let body = html! { (ctx.header(Tab::Releases, "HEAD")) (list_page(&ctx, &all, can_edit)) };
    Ok(ctx.page("releases", body))
}

async fn new_form(State(app): St, Authed(u): Authed, AxPath((owner, name)): AxPath<(String, String)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let tags = crate::git::branch_names(&ctx.dir);
    let _ = tags;
    let repo = ctx.open()?;
    let (_, mut tags) = crate::git::refs(&repo)?;
    tags.sort_by(|a, b| b.time.cmp(&a.time));
    let names: Vec<String> = tags.into_iter().map(|t| t.name).collect();
    let body = html! { (ctx.header(Tab::Releases, "HEAD")) (new_page(&ctx, &names, "", "", "", None)) };
    Ok(ctx.page("new release", body))
}

#[derive(Deserialize)]
struct NewF {
    tag: String,
    title: String,
    body: String,
    draft: Option<String>,
    prerelease: Option<String>,
}

pub(crate) fn valid_tag(t: &str) -> bool {
    !t.is_empty() && t.len() <= 200 && !t.starts_with('-') && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/+~-".contains(&b))
}

async fn create(State(app): St, Authed(u): Authed, AxPath((owner, name)): AxPath<(String, String)>, axum::Form(f): axum::Form<NewF>) -> Res {
    let ctx = admin(&app, u.clone(), &owner, &name)?;
    let fail = |e: &str| (StatusCode::BAD_REQUEST, ctx.page("new release", html! { (ctx.header(Tab::Releases, "HEAD")) (new_page(&ctx, &[], &f.tag, &f.title, &f.body, Some(e))) })).into_response();
    if !valid_tag(f.tag.trim()) || f.title.trim().is_empty() || f.title.chars().count() > 256 || f.body.len() > MAX_BODY {
        return Ok(fail("Check the tag (letters, digits, . _ / + ~ -), a title, and notes under 64 KB."));
    }
    // The tag must exist: a release points at real history.
    let tag = f.tag.trim().to_owned();
    let (dir, t) = (ctx.dir.clone(), tag.clone());
    let exists = blocking(move || Ok(crate::gitops::rev_parse(&dir, &format!("refs/tags/{t}")).is_some())).await?;
    if !exists {
        return Ok(fail("There's no such tag in this repository. Push the tag first."));
    }
    let id = app.db.create_release(ctx.repo.id, &tag, f.title.trim(), &f.body, u.id, f.draft.is_some(), f.prerelease.is_some())?;
    Ok(see_other(&format!("{}/releases/{id}", ctx.base())))
}

async fn view(State(app): St, Viewer(v): Viewer, AxPath((owner, name, id)): AxPath<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let can_edit = ctx.access >= Access::Write && !ctx.repo.archived;
    let r = app.db.release(ctx.repo.id, id)?.ok_or(Err::NotFound)?;
    if r.draft && !can_edit {
        return Err(Err::NotFound);
    }
    let assets = app.db.release_assets(id)?;
    let body = html! { (ctx.header(Tab::Releases, "HEAD")) (view_page(&ctx, &r, &assets, can_edit, None)) };
    Ok(ctx.page(&r.title, body))
}

#[derive(Deserialize)]
struct EditF {
    title: String,
    body: String,
    draft: Option<String>,
    prerelease: Option<String>,
}

async fn edit(State(app): St, Authed(u): Authed, AxPath((owner, name, id)): AxPath<(String, String, i64)>, axum::Form(f): axum::Form<EditF>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    if f.title.trim().is_empty() || f.title.chars().count() > 256 || f.body.len() > MAX_BODY {
        return Ok((StatusCode::BAD_REQUEST, ctx.page("release", html! { (ctx.header(Tab::Releases, "HEAD")) p.flash { "Title or notes too long." } })).into_response());
    }
    app.db.update_release(ctx.repo.id, id, f.title.trim(), &f.body, f.draft.is_some(), f.prerelease.is_some())?;
    Ok(see_other(&format!("{}/releases/{id}", ctx.base())))
}

async fn delete(State(app): St, Authed(u): Authed, AxPath((owner, name, id)): AxPath<(String, String, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    app.db.delete_release(ctx.repo.id, id)?;
    let _ = std::fs::remove_dir_all(asset_dir(&app, &ctx.repo, id));
    Ok(see_other(&format!("{}/releases", ctx.base())))
}

async fn upload(State(app): St, Authed(u): Authed, AxPath((owner, name, id)): AxPath<(String, String, i64)>, mut mp: Multipart) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let fail = |e: &str| -> Res {
        let r = app.db.release(ctx.repo.id, id)?.ok_or(Err::NotFound)?;
        let assets = app.db.release_assets(id)?;
        Ok((StatusCode::BAD_REQUEST, ctx.page("release", html! { (ctx.header(Tab::Releases, "HEAD")) (view_page(&ctx, &r, &assets, true, Some(e))) })).into_response())
    };
    if app.db.release(ctx.repo.id, id)?.is_none() {
        return Err(Err::NotFound);
    }
    let dir = asset_dir(&app, &ctx.repo, id);
    std::fs::create_dir_all(&dir).map_err(|e| Err::Internal(e.to_string()))?;
    while let Some(mut field) = mp.next_field().await.map_err(|e| Err::Internal(e.to_string()))? {
        if field.name() != Some("file") {
            continue;
        }
        let Some(fname) = field.file_name().and_then(safe_asset_name) else {
            return fail("File names may use letters, digits and . _ - + ~ only.");
        };
        let mut file = match tokio::fs::File::create(dir.join(&fname)).await {
            Ok(f) => f,
            Err(e) => return Err(Err::Internal(e.to_string())),
        };
        let mut size: u64 = 0;
        loop {
            match field.chunk().await {
                Ok(Some(c)) => {
                    size += c.len() as u64;
                    if size > MAX_ASSET {
                        let _ = tokio::fs::remove_file(dir.join(&fname)).await;
                        return fail("Assets are limited to 200 MB.");
                    }
                    if file.write_all(&c).await.is_err() {
                        return Err(Err::Internal("write failed".into()));
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(Err::Internal(e.to_string())),
            }
        }
        app.db.add_asset(id, &fname, size as i64)?;
    }
    Ok(see_other(&format!("{}/releases/{id}", ctx.base())))
}

async fn asset_delete(State(app): St, Authed(u): Authed, AxPath((owner, name, id, aid)): AxPath<(String, String, i64, i64)>) -> Res {
    let ctx = admin(&app, u, &owner, &name)?;
    let Some((_, fname, _)) = app.db.asset(ctx.repo.id, aid)? else { return Err(Err::NotFound) };
    app.db.c().execute("DELETE FROM release_assets WHERE id = ?1", [aid])?;
    let _ = std::fs::remove_file(asset_dir(&app, &ctx.repo, id).join(&fname));
    Ok(see_other(&format!("{}/releases/{id}", ctx.base())))
}

async fn asset_download(State(app): St, Viewer(v): Viewer, AxPath((owner, name, aid)): AxPath<(String, String, i64)>) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let Some((rid, fname, _)) = app.db.asset(ctx.repo.id, aid)? else { return Err(Err::NotFound) };
    let path = asset_dir(&app, &ctx.repo, rid).join(&fname);
    let Ok(f) = tokio::fs::File::open(&path).await else { return Err(Err::NotFound) };
    app.db.count_download(aid)?;
    let mut res = Body::from_stream(ReaderStream::with_capacity(f, 64 * 1024)).into_response();
    let disp = format!("attachment; filename=\"{}\"", fname.replace('"', ""));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&disp).map_err(|_| Err::NotFound)?);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(res)
}

/// `git archive --format=tar.gz <rev>` streamed to the client.
async fn archive(State(app): St, Viewer(v): Viewer, AxPath((owner, name, rev)): AxPath<(String, String, String)>, req: Request) -> Res {
    let ctx = Ctx::load(&app, v, &owner, &name)?;
    let _ = req;
    let dir = ctx.dir.clone();
    let rev = rev.trim_end_matches(".tar.gz").trim_end_matches(".tgz").to_owned();
    if rev.starts_with('-') || rev.contains("..") {
        return Err(Err::NotFound);
    }
    let mut child = tokio::process::Command::new("git")
        .arg("--git-dir")
        .arg(&dir)
        .args(["archive", "--format=tar.gz", "--prefix=/"])
        .arg(&rev)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .env_remove("GIT_DIR")
        .spawn()
        .map_err(|e| Err::Internal(e.to_string()))?;
    let Some(stdout) = child.stdout.take() else { return Err(Err::NotFound) };
    tokio::spawn(async move { let _ = child.wait().await; });
    let mut res = Body::from_stream(ReaderStream::with_capacity(stdout, 64 * 1024)).into_response();
    let name = format!("{}-{rev}.tar.gz", ctx.repo.name).replace(['/', '\\'], "-");
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/{owner}/{name}/releases", get(list))
        .route("/{owner}/{name}/releases/new", get(new_form).post(create))
        .route("/{owner}/{name}/releases/{id}", get(view))
        .route("/{owner}/{name}/releases/{id}/edit", post(edit))
        .route("/{owner}/{name}/releases/{id}/delete", post(delete))
        .route("/{owner}/{name}/releases/{id}/assets", post(upload))
        .route("/{owner}/{name}/releases/{id}/assets/{aid}/delete", post(asset_delete))
        .route("/{owner}/{name}/releases/assets/{aid}", get(asset_download))
        .route("/{owner}/{name}/archive/{rev}", get(archive))
}
