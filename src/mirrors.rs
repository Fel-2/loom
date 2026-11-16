//! Pull mirrors: periodically fetch refs from an upstream URL. loom updates
//! the repo in place; pushes to a mirrored repo are refused.

use std::sync::Arc;
use std::time::Duration;

use crate::app::App;
use crate::gitops;

/// Server task: every minute, sync the mirrors that are due.
pub async fn run(app: Arc<App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Cheap check on the async thread; real work goes to the pool.
        let Ok(due) = app.db.due_mirrors() else { continue };
        if due.is_empty() {
            continue;
        }
        let a = app.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for (repo_id, url, _) in due {
                sync(&a, repo_id, &url);
            }
        })
        .await;
    }
}

/// One fetch; records the outcome for the settings page.
pub fn sync(app: &App, repo_id: i64, url: &str) {
    let Ok(Some(repo)) = app.db.repo_by_id(repo_id) else { return };
    let dir = app.repo_dir(&repo);
    let (status, message) = match gitops::mirror_fetch(&dir, url) {
        Ok(out) => {
            crate::git::fix_head(&dir);
            let _ = gitops::write_commit_graph(&dir);
            ("ok", out.chars().take(500).collect::<String>())
        }
        Err(e) => {
            eprintln!("loom: mirror {}/{}: {e}", repo.owner, repo.name);
            ("error", e.chars().take(500).collect::<String>())
        }
    };
    if let Err(e) = app.db.mark_mirror_run(repo_id, status, &message) {
        eprintln!("loom: mirror status: {e}");
    }
    if status == "ok" {
        let _ = app.db.add_activity(repo_id, None, "mirror", message.split('\n').next().unwrap_or(""));
    }
}

/// Settings UI for a repo's mirror (rendered inside the settings page).
pub fn section(repo: &crate::db::Repo, db: &crate::db::Db, sbase: &str) -> maud::Markup {
    use maud::html;
    let m = db.mirror(repo.id).ok().flatten();
    html! {
        section.card.flush {
            div.card-head { "pull mirror" @if repo.mirror { span.badge { "read-only" } } }
            @if let Some((url, hours, last, status, message)) = &m {
                div.pad.stack-tight {
                    div { code { (url) } }
                    div.small.muted {
                        "every " (hours) " h · "
                        @if *last == 0 { "never synced" } @else { "last synced " (crate::ui::ago(*last)) }
                        @if !status.is_empty() {
                            " · " span class=@if status == "ok" { "add" } @else { "del" } { (status) }
                        }
                    }
                    @if !message.is_empty() { pre.small.muted { (message) } }
                    div.row {
                        form.inline method="post" action={ (sbase) "/mirror/sync" } { button.btn { "sync now" } }
                        form.inline method="post" action={ (sbase) "/mirror/delete" } { button.link.danger { "remove mirror" } }
                    }
                }
            } @else {
                form.row-form method="post" action={ (sbase) "/mirror" } {
                    input name="url" placeholder="https://upstream/repo.git" required maxlength="2000";
                    input name="hours" type="number" min="1" max="168" value="6" style="width:5rem" title="hours between syncs";
                    button.btn { "mirror this repo" }
                }
                p.pad.muted.small { "loom fetches all branches and tags. Pushing to this repo will be refused." }
            }
        }
    }
}
