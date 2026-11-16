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

/// First lines of commits in old..new (newest last), for push payloads.
fn pushed_commits(dir: &Path, old: &str, new: &str) -> Vec<(String, String)> {
    if zero(new) {
        return Vec::new();
    }
    let range = if zero(old) { new.to_owned() } else { format!("{old}..{new}") };
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["log", "--reverse", "-n", "20", "--format=%H%x00%B%x1e", "--end-of-options", &range])
        .env_remove("GIT_DIR")
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .split('\x1e')
        .filter_map(|rec| {
            let (sha, msg) = rec.trim_start_matches('\n').split_once('\0')?;
            gitops::is_sha(sha).then(|| (sha.to_owned(), msg.trim_end().to_owned()))
        })
        .collect()
}

pub fn after_update(db: &Db, data: &Path, url: &str, repo: &Repo, pusher: Option<&User>, updates: &[Update]) {
    let dir = repo_dir(data, repo);
    let head = default_ref(&dir);
    let actor = pusher.map(|u| u.id);
    let bus = crate::events::Bus { db, url };
    if updates.iter().any(|u| u.refname.starts_with("refs/heads/") && !zero(&u.new)) {
        if let Err(e) = gitops::write_commit_graph(&dir) {
            eprintln!("loom: commit-graph: {e}");
        }
    }
    for u in updates {
        if u.refname.starts_with("refs/heads/") || u.refname.starts_with("refs/tags/") {
            let commits = pushed_commits(&dir, &u.old, &u.new);
            crate::events::emit(&bus, crate::events::Ev::Push { repo, pusher, refname: &u.refname, before: &u.old, after: &u.new, commits });
        }
        let Some(branch) = u.refname.strip_prefix("refs/heads/") else { continue };
        if u.refname == head && !zero(&u.new) {
            link_commits(db, &dir, repo.id, actor, &u.old, &u.new);
        }
        if let Ok(prs) = db.open_pulls_from(repo.id, branch) {
            for pr in prs {
                if sync_head(db, data, repo, &dir, &pr, actor, u) {
                    if let (Ok(Some(base)), Ok(Some(issue))) = (db.repo_by_id(pr.repo_id), db.issue(pr.repo_id, pr.number)) {
                        if let Ok(Some(p)) = db.pull(base.id, pr.number) {
                            crate::events::emit(&bus, crate::events::Ev::Pull { repo: &base, actor: pusher, issue: &issue, pull: &p, action: "synchronize" });
                        }
                    }
                }
            }
        }
        if !zero(&u.new) {
            if let Ok(prs) = db.open_pulls_into(repo.id, branch) {
                for pr in prs {
                    if gitops::is_ancestor(&dir, &pr.head_sha, &u.new) {
                        merged(db, url, repo, &pr, pusher, &u.new, "push");
                    }
                }
            }
        }
    }
}

/// Mark merged, log it, announce it, and close issues the PR says it fixes.
pub fn merged(db: &Db, url: &str, repo: &Repo, pr: &crate::pulls_db::Pull, by: Option<&User>, sha: &str, how: &str) {
    let actor = by.map(|u| u.id);
    if !db.mark_merged(pr.issue_id, actor, sha).unwrap_or(false) {
        return;
    }
    let _ = db.add_event(pr.issue_id, actor, "merged", &format!("{sha}\t{}\t{how}", pr.base_branch), None);
    let Ok(Some(issue)) = db.issue(repo.id, pr.number) else { return };
    if let Ok(Some(p)) = db.pull(repo.id, pr.number) {
        let bus = crate::events::Bus { db, url };
        crate::events::emit(&bus, crate::events::Ev::Pull { repo, actor: by, issue: &issue, pull: &p, action: "merged" });
    }
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
/// Returns true when a new revision was recorded.
fn sync_head(db: &Db, data: &Path, head_repo: &Repo, head_dir: &Path, pr: &crate::pulls_db::Pull, actor: Option<i64>, u: &Update) -> bool {
    if zero(&u.new) {
        let _ = db.add_event(pr.issue_id, actor, "head_deleted", &pr.head_branch, None);
        return false;
    }
    if u.new == pr.head_sha {
        return false;
    }
    let Ok(Some(base)) = db.repo_by_id(pr.repo_id) else { return false };
    let base_dir = repo_dir(data, &base);
    let pin = format!("refs/pull/{}/head", pr.number);
    let synced = if base.id == head_repo.id {
        gitops::force_ref(&base_dir, &pin, &u.new)
    } else {
        gitops::fetch_branch(&base_dir, head_dir, &pr.head_branch, &pin)
    };
    if let Err(e) = synced {
        eprintln!("loom: sync PR #{}: {e}", pr.number);
        return false;
    }
    let forced = !gitops::is_ancestor(&base_dir, &pr.head_sha, &u.new);
    let base_tip = gitops::branch_sha(&base_dir, &pr.base_branch).unwrap_or_default();
    let base_sha = gitops::merge_base(&base_dir, &base_tip, &u.new).unwrap_or_default();
    let Ok(n) = db.add_revision(pr.issue_id, &u.new, &base_sha, forced, actor) else { return false };
    let count = if forced { 0 } else { gitops::rev_list(&base_dir, &pr.head_sha, &u.new, 1000).len() };
    let _ = db.add_event(
        pr.issue_id,
        actor,
        "pushed",
        &format!("{n}\t{}\t{}\t{count}", u.new, if forced { "force" } else { "" }),
        Some(&format!("rev:{}:{n}", pr.issue_id)),
    );
    true
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
