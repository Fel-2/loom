//! Git plumbing via subprocess for operations gix doesn't cover well:
//! merges, ref updates, cross-repo fetch, range-diff. All take a bare repo
//! path and never need a worktree.

use std::path::Path;
use std::process::{Command, Output};

pub struct Ident {
    pub name: String,
    pub email: String,
}

impl Ident {
    pub fn of(u: &crate::db::User) -> Self {
        let email = if u.email.is_empty() { format!("{}@users.noreply.loom", u.name) } else { u.email.clone() };
        Ident { name: u.name.clone(), email }
    }
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--git-dir").arg(dir);
    // Never let a user's or system config change plumbing behavior.
    c.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_TERMINAL_PROMPT", "0");
    // Inside hooks, receive-pack exports these for *its* repo; they'd
    // silently redirect operations on any other repo.
    for v in ["GIT_DIR", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_QUARANTINE_PATH", "GIT_INDEX_FILE", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
    c
}

fn out(o: std::io::Result<Output>) -> Result<Output, String> {
    o.map_err(|e| format!("spawn git: {e}"))
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).trim().to_owned()
}

pub fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `git check-ref-format --branch`, without ever treating input as an option.
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.len() <= 200
        && Command::new("git").args(["check-ref-format", "--branch", name]).output().is_ok_and(|o| o.status.success())
}

/// Commit id for a rev, or None.
pub fn rev_parse(dir: &Path, rev: &str) -> Option<String> {
    if rev.starts_with('-') {
        return None;
    }
    let o = git(dir).args(["rev-parse", "--verify", "--quiet", "--end-of-options"]).arg(format!("{rev}^{{commit}}")).output().ok()?;
    let s = stdout(&o);
    (o.status.success() && is_sha(&s)).then_some(s)
}

pub fn branch_sha(dir: &Path, branch: &str) -> Option<String> {
    if branch.starts_with('-') {
        return None;
    }
    crate::git::branch_tip(dir, branch).map(|id| id.to_string())
}

/// Incremental commit-graph (with changed-path Bloom filters): speeds up
/// history walks and path-filtered logs. Cheap when little changed.
pub fn write_commit_graph(dir: &Path) -> Result<(), String> {
    let o = out(git(dir).args(["commit-graph", "write", "--reachable", "--split", "--changed-paths"]).output())?;
    if o.status.success() { Ok(()) } else { Err(stderr(&o)) }
}

pub fn merge_base(dir: &Path, a: &str, b: &str) -> Option<String> {
    let o = git(dir).args(["merge-base", "--end-of-options", a, b]).output().ok()?;
    let s = stdout(&o);
    (o.status.success() && is_sha(&s)).then_some(s)
}

pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir).args(["merge-base", "--is-ancestor", "--end-of-options", a, b]).status().is_ok_and(|s| s.success())
}

/// Commits in `head` not in `base`, oldest first (capped).
pub fn rev_list(dir: &Path, base: &str, head: &str, max: usize) -> Vec<String> {
    let o = git(dir)
        .args(["rev-list", "--reverse", "--topo-order", "-n", &max.to_string(), "--end-of-options"])
        .arg(format!("{base}..{head}"))
        .output();
    match o {
        Ok(o) if o.status.success() => stdout(&o).lines().filter(|l| is_sha(l)).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// Compare-and-swap ref update (`old` = None means "must not exist").
pub fn update_ref(dir: &Path, refname: &str, new: &str, old: Option<&str>) -> Result<(), String> {
    let zero = "0".repeat(40);
    let o = out(git(dir).args(["update-ref", "-m", "loom", refname, new, old.unwrap_or(&zero)]).output())?;
    if o.status.success() { Ok(()) } else { Err(stderr(&o)) }
}

/// Set a ref regardless of its current value (loom-managed refs only).
pub fn force_ref(dir: &Path, refname: &str, new: &str) -> Result<(), String> {
    let o = out(git(dir).args(["update-ref", "-m", "loom", refname, new]).output())?;
    if o.status.success() { Ok(()) } else { Err(stderr(&o)) }
}

pub fn delete_ref(dir: &Path, refname: &str, old: &str) -> Result<(), String> {
    let o = out(git(dir).args(["update-ref", "-d", refname, old]).output())?;
    if o.status.success() { Ok(()) } else { Err(stderr(&o)) }
}

/// Remove a loom-managed scratch ref, whatever it points to.
pub fn drop_ref(dir: &Path, refname: &str) {
    let _ = git(dir).args(["update-ref", "-d", refname]).status();
}

/// Copy `src_branch` of `src` into `dst` as `dst_ref` (objects + ref).
pub fn fetch_branch(dst: &Path, src: &Path, src_branch: &str, dst_ref: &str) -> Result<(), String> {
    let o = out(git(dst)
        .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--end-of-options"])
        .arg(src)
        .arg(format!("+refs/heads/{src_branch}:{dst_ref}"))
        .output())?;
    if o.status.success() { Ok(()) } else { Err(stderr(&o)) }
}

pub enum MergeCheck {
    Clean { tree: String },
    Conflicts(Vec<String>),
}

/// Would merging `head` into `base` be clean? Computes the merged tree
/// without touching any ref.
pub fn merge_check(dir: &Path, base: &str, head: &str) -> Result<MergeCheck, String> {
    let o = out(git(dir).args(["merge-tree", "--write-tree", "--name-only", "--no-messages", "--end-of-options", base, head]).output())?;
    let text = stdout(&o);
    let mut lines = text.lines();
    let tree = lines.next().unwrap_or("").to_owned();
    match o.status.code() {
        Some(0) if is_sha(&tree) => Ok(MergeCheck::Clean { tree }),
        Some(1) => Ok(MergeCheck::Conflicts(lines.filter(|l| !l.is_empty()).map(str::to_owned).collect())),
        _ => Err(format!("merge-tree failed: {}", stderr(&o))),
    }
}

pub fn commit_tree(dir: &Path, tree: &str, parents: &[&str], msg: &str, author: &Ident, committer: &Ident) -> Result<String, String> {
    let mut c = git(dir);
    c.arg("commit-tree").arg(tree);
    for p in parents {
        c.arg("-p").arg(p);
    }
    c.arg("-m").arg(msg);
    c.env("GIT_AUTHOR_NAME", &author.name).env("GIT_AUTHOR_EMAIL", &author.email);
    c.env("GIT_COMMITTER_NAME", &committer.name).env("GIT_COMMITTER_EMAIL", &committer.email);
    let o = out(c.output())?;
    let s = stdout(&o);
    if o.status.success() && is_sha(&s) { Ok(s) } else { Err(stderr(&o)) }
}

/// Replay the commits of `head_ref` not in `onto` on top of `onto`; returns
/// the new tip. Fails (without changing anything) on conflicts.
pub fn replay(dir: &Path, onto: &str, head_ref: &str, committer: &Ident) -> Result<String, String> {
    let o = out(git(dir)
        // print: never let replay move refs itself (its default since 2.5x).
        // --ref: refs outside refs/heads aren't reported unless named.
        .args(["replay", "--ref-action=print", "--onto", onto])
        .arg(format!("--ref={head_ref}"))
        .arg("--end-of-options")
        .arg(format!("{onto}..{head_ref}"))
        .env("GIT_COMMITTER_NAME", &committer.name)
        .env("GIT_COMMITTER_EMAIL", &committer.email)
        .output())?;
    if !o.status.success() {
        return Err("the commits don't rebase cleanly onto the base branch".into());
    }
    // Output: "update <ref> <new> <old>"
    stdout(&o)
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next() == Some("update") && it.next() == Some(head_ref)).then(|| it.next().map(str::to_owned)).flatten()
        })
        .filter(|s| is_sha(s))
        .ok_or_else(|| "git replay produced no result".into())
}

/// `git range-diff` between two revisions of a PR (plain text, no color).
pub fn range_diff(dir: &Path, base1: &str, head1: &str, base2: &str, head2: &str) -> Result<String, String> {
    let o = out(git(dir)
        .args(["range-diff", "--no-color", "--end-of-options"])
        .arg(format!("{base1}..{head1}"))
        .arg(format!("{base2}..{head2}"))
        .output())?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).into_owned()) } else { Err(stderr(&o)) }
}

/// Fixed-string, case-insensitive `git grep` at `rev`: (path, line, text).
/// Binary files skipped; results capped.
pub fn grep(dir: &Path, rev: &str, needle: &str, max: usize) -> Vec<(String, u32, String)> {
    if needle.is_empty() || !is_sha(rev) {
        return Vec::new();
    }
    let o = git(dir)
        .args(["grep", "-n", "-I", "-F", "-i", "--max-count=5", "--no-color", "-e", needle, rev, "--"])
        .output();
    let Ok(o) = o else { return Vec::new() };
    let prefix = format!("{rev}:");
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.strip_prefix(&prefix)?;
            let (path, rest) = l.split_once(':')?;
            let (n, text) = rest.split_once(':')?;
            Some((path.to_owned(), n.parse().ok()?, text.chars().take(300).collect()))
        })
        .take(max)
        .collect()
}

/// Commits touching `path` (first-parent semantics like `git log -- path`),
/// newest first. Uses commit-graph Bloom filters: ~10× faster than walking
/// trees ourselves on large histories. Pathspec magic is disabled.
pub fn path_log(dir: &Path, rev: &str, path: &str, skip: usize, limit: usize) -> Vec<String> {
    if !is_sha(rev) || path.is_empty() {
        return Vec::new();
    }
    let o = git(dir)
        .arg("--literal-pathspecs")
        .args(["log", "--format=%H", &format!("--skip={skip}"), "-n", &limit.to_string(), rev, "--", path])
        .output();
    match o {
        Ok(o) if o.status.success() => stdout(&o).lines().filter(|l| is_sha(l)).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}
