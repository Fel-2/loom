//! Read-only git access via gitoxide. Everything here is blocking; callers run
//! it on the blocking pool.

use std::path::Path;

use gix::bstr::{BStr, ByteSlice};
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::object::tree::EntryKind;
use gix::prelude::ObjectIdExt;
use gix::ObjectId;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    NotFound,
    Git(String),
}

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Self {
        Error::Git(e.to_string())
    }
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_submodule: bool,
}

pub struct CommitInfo {
    pub id: ObjectId,
    pub summary: String,
    pub message: String,
    pub author: String,
    pub email: String,
    pub time: i64,
    pub parents: Vec<ObjectId>,
}

pub struct RefInfo {
    pub name: String,
    pub target: ObjectId,
    pub time: i64,
    pub summary: String,
}

pub enum Line {
    Ctx(u32, u32, String),
    Add(u32, String),
    Del(u32, String),
    Hunk(String),
}

pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: char,
    pub binary: bool,
    pub adds: u32,
    pub dels: u32,
    pub lines: Vec<Line>,
}

/// `git init --bare` with `main` as the initial branch.
pub fn init_bare(dir: &Path) -> std::result::Result<(), String> {
    let parent = dir.parent().ok_or("bad repo path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("git")
        .args(["init", "--quiet", "--bare", "--initial-branch=main"])
        .arg(dir)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(())
}

/// Local bare clone for forks; objects are hardlinked, refs copied.
pub fn clone_bare(src: &Path, dst: &Path) -> std::result::Result<(), String> {
    let parent = dst.parent().ok_or("bad repo path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("git")
        .args(["clone", "--quiet", "--bare", "--local"])
        .arg(src)
        .arg(dst)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    // A fork must not keep pointing back at the parent as `origin`.
    let _ = std::process::Command::new("git").arg("--git-dir").arg(dst).args(["remote", "remove", "origin"]).status();
    Ok(())
}

/// Point HEAD at an existing local branch.
pub fn set_default_branch(dir: &Path, branch: &str) -> std::result::Result<(), String> {
    let repo = open(dir).map_err(|_| "cannot open repository".to_string())?;
    let full = format!("refs/heads/{branch}");
    if repo.try_find_reference(full.as_str()).ok().flatten().is_none() {
        return Err(format!("no branch named '{branch}'"));
    }
    let st = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(["symbolic-ref", "HEAD", &full])
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err("git symbolic-ref failed".into()) }
}

pub fn branch_names(dir: &Path) -> Vec<String> {
    let Ok(repo) = open(dir) else { return Vec::new() };
    let Ok(platform) = repo.references() else { return Vec::new() };
    let Ok(iter) = platform.local_branches() else { return Vec::new() };
    let mut v: Vec<String> = iter.filter_map(|r| r.ok()).map(|r| r.name().shorten().to_string()).collect();
    v.sort();
    v
}

/// After a push: if HEAD points at a branch that doesn't exist (e.g. first push
/// was `master` into a `main` repo), repoint it at an existing branch.
pub fn fix_head(dir: &Path) {
    let Ok(repo) = open(dir) else { return };
    if repo.head_commit().is_ok() {
        return;
    }
    let Ok(platform) = repo.references() else { return };
    let Ok(branches) = platform.local_branches() else { return };
    let names: Vec<String> = branches.filter_map(|r| r.ok()).map(|r| r.name().as_bstr().to_string()).collect();
    let pick = ["refs/heads/main", "refs/heads/master"]
        .iter()
        .find(|n| names.iter().any(|b| b == *n))
        .map(|s| s.to_string())
        .or_else(|| names.first().cloned());
    if let Some(target) = pick {
        let _ = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["symbolic-ref", "HEAD", &target])
            .status();
    }
}

/// Open a repo, reusing a handle cached on this thread (config, refs store,
/// pack indexes load once per repo per worker thread, not per request).
/// Worker threads exit after 10 s idle, taking their cache with them, so idle
/// memory is unaffected. Handles see new pushes: refs are read from disk and
/// the object store rescans packs on a miss. An entry is dropped when the
/// directory was replaced (rename/delete/recreate changes its inode).
pub fn open(path: &Path) -> Result<gix::Repository> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    const MAX_CACHED: usize = 16;
    thread_local! {
        static CACHE: RefCell<HashMap<std::path::PathBuf, (u64, gix::Repository)>> = RefCell::new(HashMap::new());
    }
    let ino = std::fs::metadata(path).map_err(|_| Error::NotFound)?.ino();
    if let Some(r) = CACHE.with(|c| c.borrow().get(path).filter(|(i, _)| *i == ino).map(|(_, r)| r.clone())) {
        return Ok(r);
    }
    let mut repo = gix::open(path)?;
    // Objects are read once per request; a small cache is plenty and keeps RSS low.
    repo.object_cache_size(Some(512 * 1024));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= MAX_CACHED {
            c.clear();
        }
        c.insert(path.to_path_buf(), (ino, repo.clone()));
    });
    Ok(repo)
}

/// Name of the branch HEAD points to, if any.
pub fn default_branch(repo: &gix::Repository) -> Option<String> {
    let head = repo.head_name().ok()??;
    Some(head.shorten().to_string())
}

/// Split `rest` (e.g. `feature/x/src/main.rs`) into a resolvable rev and a path.
/// Shortest matching prefix wins, like git's own disambiguation.
pub fn split_rev<'a>(repo: &gix::Repository, rest: &'a str) -> Result<(ObjectId, &'a str, &'a str)> {
    let rest = rest.trim_matches('/');
    let mut idx = 0;
    loop {
        let end = rest[idx..].find('/').map_or(rest.len(), |i| idx + i);
        let rev = &rest[..end];
        if let Ok(id) = resolve_commit(repo, rev) {
            let path = rest.get(end + 1..).unwrap_or("");
            return Ok((id, rev, path));
        }
        if end == rest.len() {
            return Err(Error::NotFound);
        }
        idx = end + 1;
    }
}

pub fn resolve_commit(repo: &gix::Repository, rev: &str) -> Result<ObjectId> {
    if rev.is_empty() || rev.contains("..") || rev.starts_with('-') {
        return Err(Error::NotFound);
    }
    let id = repo.rev_parse_single(BStr::new(rev)).map_err(|_| Error::NotFound)?;
    Ok(id.object()?.peel_to_commit()?.id)
}

pub fn head_commit(repo: &gix::Repository) -> Option<ObjectId> {
    repo.head_commit().ok().map(|c| c.id)
}

fn commit_info(c: &gix::Commit<'_>) -> Result<CommitInfo> {
    let msg = c.message_raw()?.to_str_lossy().into_owned();
    let author = c.author()?;
    Ok(CommitInfo {
        id: c.id,
        summary: msg.lines().next().unwrap_or("").to_owned(),
        message: msg.trim_end().to_owned(),
        author: author.name.to_str_lossy().into_owned(),
        email: author.email.to_str_lossy().into_owned(),
        time: author.time()?.seconds,
        parents: c.parent_ids().map(|p| p.detach()).collect(),
    })
}

/// Tip of `refs/heads/<branch>` via gix (no subprocess).
pub fn branch_tip(dir: &Path, branch: &str) -> Option<ObjectId> {
    let repo = open(dir).ok()?;
    let mut r = repo.try_find_reference(format!("refs/heads/{branch}").as_str()).ok()??;
    Some(r.peel_to_commit().ok()?.id)
}

pub fn parse_id(hex: &str) -> Option<ObjectId> {
    ObjectId::from_hex(hex.as_bytes()).ok()
}

pub fn commit(repo: &gix::Repository, id: ObjectId) -> Result<CommitInfo> {
    commit_info(&repo.find_commit(id)?)
}

/// Directory listing at `path` (empty = root). Dirs first, then files.
pub fn tree(repo: &gix::Repository, id: ObjectId, path: &str) -> Result<Vec<Entry>> {
    let root = repo.find_commit(id)?.tree()?;
    let tree = if path.is_empty() {
        root
    } else {
        let e = root.lookup_entry_by_path(path)?.ok_or(Error::NotFound)?;
        if !e.mode().is_tree() {
            return Err(Error::NotFound);
        }
        e.object()?.into_tree()
    };
    let mut out: Vec<Entry> = tree
        .iter()
        .filter_map(|e| e.ok())
        .map(|e| {
            let kind = e.mode().kind();
            Entry {
                name: e.filename().to_str_lossy().into_owned(),
                is_dir: kind == EntryKind::Tree,
                is_submodule: kind == EntryKind::Commit,
            }
        })
        .collect();
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}

pub enum PathKind {
    Tree,
    Blob,
}

pub fn path_kind(repo: &gix::Repository, id: ObjectId, path: &str) -> Result<PathKind> {
    if path.is_empty() {
        return Ok(PathKind::Tree);
    }
    let root = repo.find_commit(id)?.tree()?;
    let e = root.lookup_entry_by_path(path)?.ok_or(Error::NotFound)?;
    Ok(if e.mode().is_tree() { PathKind::Tree } else { PathKind::Blob })
}

/// Returns (blob id, contents).
pub fn blob(repo: &gix::Repository, id: ObjectId, path: &str) -> Result<(ObjectId, Vec<u8>)> {
    let root = repo.find_commit(id)?.tree()?;
    let e = root.lookup_entry_by_path(path)?.ok_or(Error::NotFound)?;
    if !e.mode().is_blob() {
        return Err(Error::NotFound);
    }
    let obj = e.object()?;
    let id = obj.id;
    Ok((id, obj.detach().data))
}

/// First README-like file at the tree root.
pub fn readme(repo: &gix::Repository, id: ObjectId, dir: &str) -> Option<(String, String)> {
    let entries = tree(repo, id, dir).ok()?;
    let name = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| &e.name)
        .find(|n| n.to_ascii_lowercase().starts_with("readme"))?;
    let path = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
    let (_, data) = blob(repo, id, &path).ok()?;
    Some((name.clone(), String::from_utf8_lossy(&data).into_owned()))
}

/// Commits reachable from `id`, newest first. If `path` is set, only commits
/// touching it (compared against first parent).
pub fn log(repo: &gix::Repository, id: ObjectId, path: &str, skip: usize, limit: usize) -> Result<Vec<CommitInfo>> {
    if !path.is_empty() {
        // git's own path-limited log uses changed-path Bloom filters.
        return crate::gitops::path_log(repo.git_dir(), &id.to_string(), path, skip, limit)
            .iter()
            .filter_map(|h| parse_id(h))
            .map(|cid| commit(repo, cid))
            .collect();
    }
    let walk = repo
        .rev_walk([id])
        .sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default()))
        .all()?;
    let mut out = Vec::with_capacity(limit);
    let mut seen = 0;
    for info in walk {
        let info = info?;
        let c = info.object()?;
        if !path.is_empty() && !touches(repo, &c, path)? {
            continue;
        }
        seen += 1;
        if seen <= skip {
            continue;
        }
        out.push(commit_info(&c)?);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

fn entry_id(tree: &gix::Tree<'_>, path: &str) -> Result<Option<ObjectId>> {
    Ok(tree.lookup_entry_by_path(path)?.map(|e| e.object_id()))
}

fn touches(repo: &gix::Repository, c: &gix::Commit<'_>, path: &str) -> Result<bool> {
    let now = entry_id(&c.tree()?, path)?;
    let before = match c.parent_ids().next() {
        Some(p) => entry_id(&repo.find_commit(p)?.tree()?, path)?,
        None => None,
    };
    Ok(now != before)
}

pub fn refs(repo: &gix::Repository) -> Result<(Vec<RefInfo>, Vec<RefInfo>)> {
    let platform = repo.references()?;
    let collect = |iter: gix::reference::iter::Iter<'_, '_>| -> Vec<RefInfo> {
        let mut v: Vec<RefInfo> = iter
            .filter_map(|r| r.ok())
            .filter_map(|mut r| {
                let name = r.name().shorten().to_string();
                let c = r.peel_to_commit().ok()?;
                let info = commit_info(&c).ok()?;
                Some(RefInfo { name, target: c.id, time: info.time, summary: info.summary })
            })
            .collect();
        v.sort_by(|a, b| b.time.cmp(&a.time));
        v
    };
    Ok((collect(platform.local_branches()?), collect(platform.tags()?)))
}

const MAX_DIFF_BYTES: usize = 512 * 1024;

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

/// Diff a commit against its first parent (or the empty tree).
pub fn commit_diff(repo: &gix::Repository, id: ObjectId) -> Result<Vec<FileDiff>> {
    let c = repo.find_commit(id)?;
    let parent = c.parent_ids().next().map(|p| p.detach());
    diff_commits(repo, parent, id)
}

/// Tree diff between two commits (`old = None` = empty tree).
pub fn diff_commits(repo: &gix::Repository, old: Option<ObjectId>, new: ObjectId) -> Result<Vec<FileDiff>> {
    let new_tree = repo.find_commit(new)?.tree()?;
    let old_tree = match old {
        Some(p) => Some(repo.find_commit(p)?.tree()?),
        None => None,
    };
    let changes = repo.diff_tree_to_tree(old_tree.as_ref(), &new_tree, None)?;
    let mut out = Vec::new();
    for ch in changes {
        use gix::object::tree::diff::ChangeDetached as C;
        let (status, path, old_path, old_id, new_id, mode) = match ch {
            C::Addition { location, id, entry_mode, .. } => ('A', location, None, None, Some(id), entry_mode),
            C::Deletion { location, id, entry_mode, .. } => ('D', location, None, Some(id), None, entry_mode),
            C::Modification { location, previous_id, id, entry_mode, .. } => {
                ('M', location, None, Some(previous_id), Some(id), entry_mode)
            }
            C::Rewrite { source_location, source_id, location, id, entry_mode, copy, .. } => {
                (if copy { 'C' } else { 'R' }, location, Some(source_location), Some(source_id), Some(id), entry_mode)
            }
        };
        if mode.is_tree() {
            continue;
        }
        let load = |oid: Option<ObjectId>| -> Result<Vec<u8>> {
            match oid {
                Some(o) if mode.is_blob() => Ok(o.attach(repo).object()?.detach().data),
                Some(o) => Ok(format!("Subproject commit {o}\n").into_bytes()),
                None => Ok(Vec::new()),
            }
        };
        let (a, b) = (load(old_id)?, load(new_id)?);
        let mut fd = FileDiff {
            path: path.to_str_lossy().into_owned(),
            old_path: old_path.map(|p| p.to_str_lossy().into_owned()),
            status,
            binary: false,
            adds: 0,
            dels: 0,
            lines: Vec::new(),
        };
        if is_binary(&a) || is_binary(&b) || a.len() + b.len() > MAX_DIFF_BYTES {
            fd.binary = true;
        } else {
            unified(&a, &b, &mut fd);
        }
        out.push(fd);
    }
    Ok(out)
}

fn unified(a: &[u8], b: &[u8], fd: &mut FileDiff) {
    let input = InternedInput::new(a, b);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    fd.adds = diff.count_additions();
    fd.dels = diff.count_removals();
    let text = |t: &gix::diff::blob::Token| -> String {
        let bytes: &[u8] = input.interner[*t];
        String::from_utf8_lossy(bytes).trim_end_matches(['\n', '\r']).to_owned()
    };
    const CTX: u32 = 3;
    let hunks: Vec<_> = diff.hunks().collect();
    let (nb, na) = (input.before.len() as u32, input.after.len() as u32);
    let mut i = 0;
    while i < hunks.len() {
        // Merge hunks whose context overlaps into one display group.
        let mut j = i;
        while j + 1 < hunks.len() && hunks[j + 1].before.start <= hunks[j].before.end + 2 * CTX {
            j += 1;
        }
        let bstart = hunks[i].before.start.saturating_sub(CTX);
        let astart = hunks[i].after.start - (hunks[i].before.start - bstart);
        let bend = (hunks[j].before.end + CTX).min(nb);
        let aend = (hunks[j].after.end + CTX).min(na);
        fd.lines.push(Line::Hunk(format!(
            "@@ -{},{} +{},{} @@",
            bstart + 1,
            bend - bstart,
            astart + 1,
            aend - astart
        )));
        let (mut bp, mut ap) = (bstart, astart);
        for h in &hunks[i..=j] {
            while bp < h.before.start {
                fd.lines.push(Line::Ctx(bp + 1, ap + 1, text(&input.before[bp as usize])));
                bp += 1;
                ap += 1;
            }
            for k in h.before.clone() {
                fd.lines.push(Line::Del(k + 1, text(&input.before[k as usize])));
            }
            for k in h.after.clone() {
                fd.lines.push(Line::Add(k + 1, text(&input.after[k as usize])));
            }
            bp = h.before.end;
            ap = h.after.end;
        }
        while bp < bend {
            fd.lines.push(Line::Ctx(bp + 1, ap + 1, text(&input.before[bp as usize])));
            bp += 1;
            ap += 1;
        }
        i = j + 1;
    }
}
