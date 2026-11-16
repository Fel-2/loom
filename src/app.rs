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
    /// Failed password attempts per lowercased user name: (count, window start).
    failures: Mutex<HashMap<String, (u32, i64)>>,
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
        let repos = data.join("repos");
        std::fs::create_dir_all(&repos).map_err(|e| format!("create {}: {e}", repos.display()))?;
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
            failures: Mutex::new(HashMap::new()),
        })
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

    /// Validate, record and `git init --bare` a new repo. Blocking.
    pub fn create_repo(&self, by: Option<&User>, owner: &str, name: &str, desc: &str, private: bool) -> Result<Repo, String> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        if !db::valid_name(owner) || !db::valid_name(name) {
            return Err("names may use letters, digits, '.', '_' and '-'".into());
        }
        if let Some(u) = by {
            if !self.db.can_create_under(u, owner) {
                return Err(format!("you can't create repositories under '{owner}'"));
            }
        }
        // Repos live under the owner's canonical name.
        let owner = match self.db.user_by_name(owner).map_err(|e| e.to_string())? {
            Some(u) => u.name,
            None => return Err(format!("no such user '{owner}'")),
        };
        if self.db.repo(&owner, name).map_err(|e| e.to_string())?.is_some() {
            return Err("a repository with that name already exists".into());
        }
        let id = self.db.create_repo(&owner, name, desc.trim(), private).map_err(|e| e.to_string())?;
        let repo = Repo { id, owner, name: name.to_owned(), description: desc.trim().to_owned(), private };
        let dir = self.repo_dir(&repo);
        if dir.exists() {
            // Adopt an existing bare repo dropped on disk.
            return Ok(repo);
        }
        if let Err(e) = git::init_bare(&dir) {
            let _ = self.db.delete_repo(id);
            return Err(e);
        }
        Ok(repo)
    }

    /// Resolve `owner/name` for `user`, returning `None` unless they have at least `need`.
    /// Unauthorized and missing look identical, so private repos don't leak.
    pub fn authorize(&self, user: Option<&User>, owner: &str, name: &str, need: db::Access) -> Option<(Repo, db::Access)> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let repo = self.db.repo(owner, name).ok()??;
        let access = self.db.access(user, &repo).ok()?;
        (access >= need).then_some((repo, access))
    }
}
