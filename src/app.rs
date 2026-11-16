//! Shared state and config, used by the web server, SSH server and CLI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::db::{self, Db, Repo, User};
use crate::git;

pub struct App {
    pub data: PathBuf,
    pub repos: PathBuf,
    pub db: Db,
    /// Public base URL for HTTP clone links, e.g. `https://git.example.com`.
    pub url: String,
    /// Address the HTTP server binds to.
    pub http_addr: String,
    /// Address the SSH server binds to; `None` disables SSH.
    pub ssh_addr: Option<String>,
    /// Host (and port) shown in SSH clone links.
    pub ssh_host: String,
    /// Send cookies with `Secure` (set when `url` is https).
    pub secure_cookies: bool,
    /// Path of the loom binary, for git hooks to call back into.
    pub bin: PathBuf,
    /// Failed password attempts per lowercased user name: (count, window start).
    failures: Mutex<HashMap<String, (u32, i64)>>,
}

/// The binary's path. After an in-place upgrade Linux reports the running
/// image as "<path> (deleted)"; hooks must run whatever now lives at <path>.
fn own_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("loom"));
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(p) => PathBuf::from(p),
        None => exe,
    }
}

pub enum Login {
    Ok(User),
    Bad,
    Throttled,
}

const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: i64 = 15 * 60;

impl App {
    pub fn from_env() -> Result<Self, String> {
        let data = std::env::var_os("LOOM_DATA").map_or_else(|| PathBuf::from("./data"), PathBuf::from);
        std::fs::create_dir_all(data.join("repos")).map_err(|e| format!("create {}: {e}", data.display()))?;
        // Absolute: git hooks run with the repo as cwd.
        let data = data.canonicalize().map_err(|e| format!("{}: {e}", data.display()))?;
        let repos = data.join("repos");
        let db = Db::open(&data.join("loom.db")).map_err(|e| format!("open db: {e}"))?;
        let http_addr = std::env::var("LOOM_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
        let url = std::env::var("LOOM_URL")
            .unwrap_or_else(|_| format!("http://{http_addr}"))
            .trim_end_matches('/')
            .to_owned();
        let ssh_addr = match std::env::var("LOOM_SSH_ADDR") {
            Ok(s) if s == "off" => None,
            Ok(s) => Some(s),
            Err(_) => Some("127.0.0.1:2222".into()),
        };
        let ssh_host = std::env::var("LOOM_SSH_HOST").unwrap_or_else(|_| {
            ssh_addr.clone().unwrap_or_default()
        });
        Ok(App {
            secure_cookies: url.starts_with("https://"),
            data,
            repos,
            db,
            url,
            http_addr,
            ssh_addr,
            ssh_host,
            bin: own_binary(),
            failures: Mutex::new(HashMap::new()),
        })
    }

    pub fn bus(&self) -> crate::events::Bus<'_> {
        crate::events::Bus { db: &self.db, url: &self.url }
    }

    /// Password login with per-user throttling. Slow (argon2): run off the async thread.
    pub fn login(&self, name: &str, password: &str) -> Login {
        let key = name.to_ascii_lowercase();
        let now = db::now();
        {
            let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
            if f.len() > 4096 {
                f.retain(|_, (_, t)| now - *t < FAILURE_WINDOW);
            }
            if let Some((n, t)) = f.get(&key) {
                if *n >= MAX_FAILURES && now - t < FAILURE_WINDOW {
                    return Login::Throttled;
                }
            }
        }
        match self.db.check_password(name, password) {
            Ok(Some(u)) => {
                self.failures.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                Login::Ok(u)
            }
            _ => {
                let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
                let e = f.entry(key).or_insert((0, now));
                if now - e.1 >= FAILURE_WINDOW {
                    *e = (0, now);
                }
                e.0 += 1;
                Login::Bad
            }
        }
    }

    pub fn repo_dir(&self, repo: &Repo) -> PathBuf {
        self.repos.join(&repo.owner).join(format!("{}.git", repo.name))
    }

    pub fn clone_urls(&self, repo: &Repo) -> (String, Option<String>) {
        let http = format!("{}/{}/{}.git", self.url, repo.owner, repo.name);
        let ssh = self.ssh_addr.as_ref().map(|_| {
            format!("ssh://git@{}/{}/{}.git", self.ssh_host, repo.owner, repo.name)
        });
        (http, ssh)
    }

    /// Check the target namespace and return the owner's canonical name.
    fn target_owner(&self, by: Option<&User>, owner: &str, name: &str) -> Result<String, String> {
        if !db::valid_name(owner) || !db::valid_name(name) {
            return Err("names may use letters, digits, '.', '_' and '-'".into());
        }
        if let Some(u) = by {
            if !self.db.can_create_under(u, owner) {
                return Err(format!("you can't create repositories under '{owner}'"));
            }
        }
        let owner = match self.db.owner(owner).map_err(|e| e.to_string())? {
            Some(o) => o.name,
            None => return Err(format!("no such user or org '{owner}'")),
        };
        if self.db.repo(&owner, name).map_err(|e| e.to_string())?.is_some() {
            return Err(format!("{owner}/{name} already exists"));
        }
        Ok(owner)
    }

    /// Validate, record and `git init --bare` a new repo. Blocking.
    pub fn create_repo(&self, by: Option<&User>, owner: &str, name: &str, desc: &str, private: bool) -> Result<Repo, String> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner = self.target_owner(by, owner, name)?;
        let id = self.db.create_repo(&owner, name, desc.trim(), private).map_err(|e| e.to_string())?;
        let repo = self.db.repo_by_id(id).map_err(|e| e.to_string())?.ok_or("repo vanished")?;
        let dir = self.repo_dir(&repo);
        if dir.exists() {
            // Adopt an existing bare repo dropped on disk.
            self.auto_watch(by, &repo);
            return Ok(repo);
        }
        if let Err(e) = git::init_bare(&dir) {
            let _ = self.db.delete_repo(id);
            return Err(e);
        }
        self.auto_watch(by, &repo);
        Ok(repo)
    }

    /// Creator and (user) owner watch new repos.
    fn auto_watch(&self, by: Option<&User>, repo: &Repo) {
        if let Some(u) = by {
            let _ = self.db.watch(u.id, repo.id);
        }
        if let Ok(Some(owner)) = self.db.user_by_name(&repo.owner) {
            let _ = self.db.watch(owner.id, repo.id);
        }
    }

    /// Fork `src` into `owner/name`. Hardlinked objects: near-zero extra disk. Blocking.
    pub fn fork_repo(&self, by: &User, src: &Repo, owner: &str, name: &str) -> Result<Repo, String> {
        let owner = self.target_owner(Some(by), owner, name)?;
        let id = self.db.create_repo(&owner, name, &src.description, src.private).map_err(|e| e.to_string())?;
        self.db.set_fork_of(id, src.id).map_err(|e| e.to_string())?;
        let repo = self.db.repo_by_id(id).map_err(|e| e.to_string())?.ok_or("repo vanished")?;
        if let Err(e) = git::clone_bare(&self.repo_dir(src), &self.repo_dir(&repo)) {
            let _ = self.db.delete_repo(id);
            return Err(e);
        }
        self.auto_watch(Some(by), &repo);
        Ok(repo)
    }

    /// Rename and/or transfer. Moves the directory, then the DB row. Blocking.
    pub fn move_repo(&self, by: &User, repo: &Repo, owner: &str, name: &str) -> Result<Repo, String> {
        let same_owner = owner.eq_ignore_ascii_case(&repo.owner);
        let owner = if same_owner {
            if !db::valid_name(name) {
                return Err("names may use letters, digits, '.', '_' and '-'".into());
            }
            if !name.eq_ignore_ascii_case(&repo.name) && self.db.repo(&repo.owner, name).map_err(|e| e.to_string())?.is_some() {
                return Err(format!("{}/{name} already exists", repo.owner));
            }
            repo.owner.clone()
        } else {
            self.target_owner(Some(by), owner, name)?
        };
        let from = self.repo_dir(repo);
        let mut moved = repo.clone();
        moved.owner = owner;
        moved.name = name.to_owned();
        let to = self.repo_dir(&moved);
        if from != to {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::rename(&from, &to).map_err(|e| format!("move repository: {e}"))?;
        }
        if let Err(e) = self.db.move_repo(repo.id, &moved.owner, &moved.name) {
            let _ = std::fs::rename(&to, &from);
            return Err(e.to_string());
        }
        Ok(moved)
    }

    /// Delete: the DB row goes, the directory moves to `data/trash/` (recoverable). Blocking.
    pub fn delete_repo(&self, repo: &Repo) -> Result<(), String> {
        let trash = self.data.join("trash");
        std::fs::create_dir_all(&trash).map_err(|e| e.to_string())?;
        let dest = trash.join(format!("{}-{}-{}.git", repo.owner, repo.name, db::now()));
        let dir = self.repo_dir(repo);
        if dir.exists() {
            std::fs::rename(&dir, &dest).map_err(|e| format!("move to trash: {e}"))?;
        }
        if let Err(e) = self.db.delete_repo(repo.id) {
            let _ = std::fs::rename(&dest, &dir);
            return Err(e.to_string());
        }
        Ok(())
    }

    /// Hook scripts live once in `data/hooks` and are wired in per push via
    /// `-c core.hooksPath`; they call back into this binary.
    pub fn install_hooks(&self) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.data.join("hooks");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for hook in ["pre-receive", "post-receive"] {
            let path = dir.join(hook);
            let script = format!("#!/bin/sh\nexec \"$LOOM_BIN\" hook {hook}\n");
            if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
                std::fs::write(&path, &script).map_err(|e| e.to_string())?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Environment + config for a `git receive-pack` child so hooks can find us.
    pub fn receive_pack_setup(&self, cmd: &mut tokio::process::Command, repo: &Repo, user: Option<&User>) {
        cmd.arg("-c").arg(format!("core.hooksPath={}", self.data.join("hooks").display()));
        cmd.env("LOOM_DATA", &self.data);
        cmd.env("LOOM_URL", &self.url);
        cmd.env("LOOM_REPO_ID", repo.id.to_string());
        cmd.env("LOOM_USER", user.map_or("", |u| u.name.as_str()));
        cmd.env("LOOM_BIN", &self.bin);
    }

    /// Resolve `owner/name` for `user`, returning `None` unless they have at least `need`.
    /// Unauthorized and missing look identical, so private repos don't leak.
    pub fn authorize(&self, user: Option<&User>, owner: &str, name: &str, need: db::Access) -> Option<(Repo, db::Access)> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let repo = self.db.repo(owner, name).ok()??;
        let mut access = self.db.access(user, &repo).ok()?;
        // Archived repos are read-only for everyone (admins can unarchive).
        if repo.archived && need == db::Access::Write {
            access = access.min(db::Access::Read);
        }
        (access >= need).then_some((repo, access))
    }
}
