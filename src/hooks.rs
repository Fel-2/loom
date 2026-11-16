//! What happens after refs move — from a push (post-receive) or a web merge:
//! link/close issues from commits, sync PR heads into revisions, and mark
//! PRs merged when their head lands in the base branch.

use std::path::{Path, PathBuf};

use crate::db::{Db, Repo, User};
use crate::gitops;

pub struct Update {
    pub old: String,
    pub new: String,
    pub refname: String,
}

const MAX_COMMITS: usize = 500;

fn zero(s: &str) -> bool {
    s.bytes().all(|b| b == b'0')
}

pub fn repo_dir(data: &Path, repo: &Repo) -> PathBuf {
    data.join("repos").join(&repo.owner).join(format!("{}.git", repo.name))
}

fn default_ref(dir: &Path) -> String {
    std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["symbolic-ref", "-q", "HEAD"])
        .env_remove("GIT_DIR")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

pub fn after_update(db: &Db, data: &Path, repo: &Repo, pusher: Option<&User>, updates: &[Update]) {
    let dir = repo_dir(data, repo);
    let head = default_ref(&dir);
    let actor = pusher.map(|u| u.id);
    for u in updates {
        let Some(branch) = u.refname.strip_prefix("refs/heads/") else { continue };
        if u.refname == head && !zero(&u.new) {
            link_commits(db, &dir, repo.id, actor, &u.old, &u.new);
        }
        if let Ok(prs) = db.open_pulls_from(repo.id, branch) {
            for pr in prs {
                sync_head(db, data, repo, &dir, &pr, actor, u);
            }
        }
        if !zero(&u.new) {
            if let Ok(prs) = db.open_pulls_into(repo.id, branch) {
                for pr in prs {
                    if gitops::is_ancestor(&dir, &pr.head_sha, &u.new) {
                        merged(db, repo, &pr, actor, &u.new, "push");
                    }
                }
            }
        }
    }
}

/// Mark merged, log it, and close issues the PR says it fixes.
pub fn merged(db: &Db, repo: &Repo, pr: &crate::pulls_db::Pull, actor: Option<i64>, sha: &str, how: &str) {
    if !db.mark_merged(pr.issue_id, actor, sha).unwrap_or(false) {
        return;
    }
    let _ = db.add_event(pr.issue_id, actor, "merged", &format!("{sha}\t{}\t{how}", pr.base_branch), None);
    let Ok(Some(issue)) = db.issue(repo.id, pr.number) else { return };
    let text = format!("{}\n{}", issue.title, issue.body);
    for (r, number, closing) in crate::refs::issue_refs(&text) {
        if !closing || r.is_some() || number == pr.number {
            continue;
        }
        let Ok(Some(iid)) = db.issue_id(repo.id, number) else { continue };
        let key = format!("pr_closed:{}:{iid}", pr.issue_id);
        let _ = db.set_issue_closed(iid, true, actor, "pr_closed", &pr.number.to_string(), Some(&key));
    }
}

/// A PR's head branch moved: pin it in the base repo and record a revision.
fn sync_head(db: &Db, data: &Path, head_repo: &Repo, head_dir: &Path, pr: &crate::pulls_db::Pull, actor: Option<i64>, u: &Update) {
    if zero(&u.new) {
        let _ = db.add_event(pr.issue_id, actor, "head_deleted", &pr.head_branch, None);
        return;
    }
    if u.new == pr.head_sha {
        return;
    }
    let Ok(Some(base)) = db.repo_by_id(pr.repo_id) else { return };
    let base_dir = repo_dir(data, &base);
    let pin = format!("refs/pull/{}/head", pr.number);
    let synced = if base.id == head_repo.id {
        gitops::force_ref(&base_dir, &pin, &u.new)
    } else {
        gitops::fetch_branch(&base_dir, head_dir, &pr.head_branch, &pin)
    };
    if let Err(e) = synced {
        eprintln!("loom: sync PR #{}: {e}", pr.number);
        return;
    }
    let forced = !gitops::is_ancestor(&base_dir, &pr.head_sha, &u.new);
    let base_tip = gitops::branch_sha(&base_dir, &pr.base_branch).unwrap_or_default();
    let base_sha = gitops::merge_base(&base_dir, &base_tip, &u.new).unwrap_or_default();
    let Ok(n) = db.add_revision(pr.issue_id, &u.new, &base_sha, forced, actor) else { return };
    let count = if forced { 0 } else { gitops::rev_list(&base_dir, &pr.head_sha, &u.new, 1000).len() };
    let _ = db.add_event(
        pr.issue_id,
        actor,
        "pushed",
        &format!("{n}\t{}\t{}\t{count}", u.new, if forced { "force" } else { "" }),
        Some(&format!("rev:{}:{n}", pr.issue_id)),
    );
}

/// Commits newly on the default branch: `#N` adds a reference, `fixes #N` closes.
fn link_commits(db: &Db, dir: &Path, repo_id: i64, actor: Option<i64>, old: &str, new: &str) {
    let range = if zero(old) { new.to_owned() } else { format!("{old}..{new}") };
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["log", "--reverse", "--format=%H%x00%s%x00%B%x1e", "-n", &MAX_COMMITS.to_string(), "--end-of-options", &range])
        .env_remove("GIT_DIR")
        .output();
    let Ok(out) = out else { return };
    for rec in String::from_utf8_lossy(&out.stdout).split('\x1e') {
        let mut parts = rec.trim_start_matches('\n').splitn(3, '\0');
        let (Some(sha), Some(subject), Some(msg)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if !gitops::is_sha(sha) {
            continue;
        }
        for (repo, number, closing) in crate::refs::issue_refs(msg) {
            if repo.is_some() {
                continue;
            }
            let Ok(Some(issue_id)) = db.issue_id(repo_id, number) else { continue };
            let data = format!("{sha}\t{subject}");
            let as_ref = |db: &Db| db.add_event(issue_id, actor, "commit_ref", &data, Some(&format!("commit_ref:{sha}:{issue_id}")));
            if closing {
                match db.set_issue_closed(issue_id, true, actor, "commit_closed", &data, Some(&format!("commit_closed:{sha}:{issue_id}"))) {
                    Ok(false) => {
                        let _ = as_ref(db);
                    }
                    Ok(true) => {}
                    Err(e) => eprintln!("loom: close #{number}: {e}"),
                }
            } else {
                let _ = as_ref(db);
            }
        }
    }
}
