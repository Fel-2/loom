//! SQLite storage: accounts, keys, tokens, sessions, repo metadata, access.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub use rusqlite::Error;
pub type Result<T> = rusqlite::Result<T>;

const SCHEMA: &str = "
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    email TEXT NOT NULL DEFAULT '',
    password TEXT,
    is_admin INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE ssh_keys (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    fingerprint TEXT NOT NULL UNIQUE,
    key TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE tokens (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL,
    last_used INTEGER
);
CREATE TABLE sessions (
    hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires INTEGER NOT NULL
);
CREATE TABLE repos (
    id INTEGER PRIMARY KEY,
    owner TEXT NOT NULL COLLATE NOCASE,
    name TEXT NOT NULL COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    private INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    UNIQUE (owner, name)
);
CREATE TABLE collaborators (
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role INTEGER NOT NULL,
    PRIMARY KEY (repo_id, user_id)
);
";

pub const SESSION_DAYS: i64 = 30;

/// Names that would collide with top-level routes.
const RESERVED: &[&str] = &["static", "login", "logout", "settings", "new", "admin", "api", "explore"];

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Users, owners and repo names: `[A-Za-z0-9._-]`, max 64, no leading `.`/`-`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with(['.', '-'])
        && !s.ends_with(".git")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !RESERVED.contains(&s.to_ascii_lowercase().as_str())
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub is_admin: bool,
}

pub struct SshKey {
    pub id: i64,
    pub title: String,
    pub fingerprint: String,
    pub created: i64,
}

pub struct Token {
    pub id: i64,
    pub title: String,
    pub created: i64,
    pub last_used: Option<i64>,
}

#[derive(Clone)]
pub struct Repo {
    pub id: i64,
    pub owner: String,
    pub name: String,
    pub description: String,
    pub private: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Access {
    None = 0,
    Read = 1,
    Write = 2,
    Admin = 3,
}

impl Access {
    fn from_role(r: i64) -> Self {
        match r {
            3 => Access::Admin,
            2 => Access::Write,
            1 => Access::Read,
            _ => Access::None,
        }
    }
}

pub struct Db(Mutex<Connection>);

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("os rng");
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Secrets with 256 bits of entropy only need a fast hash at rest.
pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// Argon2id with OWASP's low-memory profile (7 MiB, t=5): logins must not
/// spike RSS by the default 19 MiB.
fn argon() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(7 * 1024, 5, 1, None).expect("params"))
}

pub fn hash_password(pw: &str) -> String {
    argon().hash_password(pw.as_bytes()).expect("argon2").to_string()
}

pub fn verify_password(pw: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| argon().verify_password(pw.as_bytes(), &h).is_ok())
}

/// Dummy hash so failed lookups take as long as real ones (no user enumeration).
fn burn_password_check(pw: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let h = DUMMY.get_or_init(|| hash_password("loom-dummy-password"));
    verify_password(pw, h);
}

fn user_row(r: &rusqlite::Row<'_>) -> Result<User> {
    Ok(User { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, is_admin: r.get(3)? })
}

fn repo_row(r: &rusqlite::Row<'_>) -> Result<Repo> {
    Ok(Repo { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, description: r.get(3)?, private: r.get(4)? })
}

const USER_COLS: &str = "id, name, email, is_admin";
const REPO_COLS: &str = "id, owner, name, description, private";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000; PRAGMA cache_size = -1024;",
        )?;
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v < 1 {
            c.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))?;
        }
        Ok(Db(Mutex::new(c)))
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- users ----

    pub fn create_user(&self, name: &str, email: &str, password: Option<&str>, admin: bool) -> Result<i64> {
        let hash = password.map(hash_password);
        let c = self.c();
        c.execute(
            "INSERT INTO users (name, email, password, is_admin, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, email, hash, admin, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        self.c()
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE name = ?1"), [name], user_row)
            .optional()
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY name"))?;
        st.query_map([], user_row)?.collect()
    }

    pub fn delete_user(&self, name: &str) -> Result<bool> {
        Ok(self.c().execute("DELETE FROM users WHERE name = ?1", [name])? > 0)
    }

    pub fn set_password(&self, user_id: i64, pw: &str) -> Result<()> {
        let h = hash_password(pw);
        let c = self.c();
        c.execute("UPDATE users SET password = ?1 WHERE id = ?2", params![h, user_id])?;
        // Changing the password signs out every other session.
        c.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    /// Slow (argon2). Run off the async thread.
    pub fn check_password(&self, name: &str, pw: &str) -> Result<Option<User>> {
        let row: Option<(User, Option<String>)> = self
            .c()
            .query_row(&format!("SELECT {USER_COLS}, password FROM users WHERE name = ?1"), [name], |r| {
                Ok((user_row(r)?, r.get(4)?))
            })
            .optional()?;
        match row {
            Some((u, Some(h))) if verify_password(pw, &h) => Ok(Some(u)),
            Some((_, Some(_))) => Ok(None),
            _ => {
                burn_password_check(pw);
                Ok(None)
            }
        }
    }

    // ---- sessions ----

    /// Returns the raw cookie value; only its hash is stored.
    pub fn create_session(&self, user_id: i64) -> Result<String> {
        let raw = random_hex(32);
        let c = self.c();
        c.execute("DELETE FROM sessions WHERE expires < ?1", [now()])?;
        c.execute(
            "INSERT INTO sessions (hash, user_id, expires) VALUES (?1, ?2, ?3)",
            params![sha256_hex(&raw), user_id, now() + SESSION_DAYS * 86400],
        )?;
        Ok(raw)
    }

    pub fn session_user(&self, raw: &str) -> Result<Option<User>> {
        self.c()
            .query_row(
                "SELECT u.id, u.name, u.email, u.is_admin FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.hash = ?1 AND s.expires > ?2",
                params![sha256_hex(raw), now()],
                user_row,
            )
            .optional()
    }

    pub fn delete_session(&self, raw: &str) -> Result<()> {
        self.c().execute("DELETE FROM sessions WHERE hash = ?1", [sha256_hex(raw)])?;
        Ok(())
    }

    // ---- tokens ----

    /// Returns the raw token (shown once); only its hash is stored.
    pub fn create_token(&self, user_id: i64, title: &str) -> Result<String> {
        let raw = format!("loom_{}", random_hex(32));
        self.c().execute(
            "INSERT INTO tokens (user_id, title, hash, created) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, title, sha256_hex(&raw), now()],
        )?;
        Ok(raw)
    }

    pub fn token_user(&self, raw: &str) -> Result<Option<User>> {
        let h = sha256_hex(raw);
        let c = self.c();
        let u = c
            .query_row(
                "SELECT u.id, u.name, u.email, u.is_admin FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
                [&h],
                user_row,
            )
            .optional()?;
        if u.is_some() {
            c.execute("UPDATE tokens SET last_used = ?1 WHERE hash = ?2", params![now(), h])?;
        }
        Ok(u)
    }

    pub fn tokens(&self, user_id: i64) -> Result<Vec<Token>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, title, created, last_used FROM tokens WHERE user_id = ?1 ORDER BY id DESC")?;
        st.query_map([user_id], |r| {
            Ok(Token { id: r.get(0)?, title: r.get(1)?, created: r.get(2)?, last_used: r.get(3)? })
        })?
        .collect()
    }

    pub fn delete_token(&self, user_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM tokens WHERE id = ?1 AND user_id = ?2", params![id, user_id])?;
        Ok(())
    }

    // ---- ssh keys ----

    pub fn add_key(&self, user_id: i64, title: &str, fingerprint: &str, key: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO ssh_keys (user_id, title, fingerprint, key, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user_id, title, fingerprint, key, now()],
        )?;
        Ok(())
    }

    pub fn keys(&self, user_id: i64) -> Result<Vec<SshKey>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, title, fingerprint, created FROM ssh_keys WHERE user_id = ?1 ORDER BY id")?;
        st.query_map([user_id], |r| {
            Ok(SshKey { id: r.get(0)?, title: r.get(1)?, fingerprint: r.get(2)?, created: r.get(3)? })
        })?
        .collect()
    }

    pub fn delete_key(&self, user_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM ssh_keys WHERE id = ?1 AND user_id = ?2", params![id, user_id])?;
        Ok(())
    }

    pub fn key_user(&self, fingerprint: &str) -> Result<Option<User>> {
        self.c()
            .query_row(
                "SELECT u.id, u.name, u.email, u.is_admin FROM ssh_keys k JOIN users u ON u.id = k.user_id
                 WHERE k.fingerprint = ?1",
                [fingerprint],
                user_row,
            )
            .optional()
    }

    // ---- repos ----

    pub fn create_repo(&self, owner: &str, name: &str, description: &str, private: bool) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO repos (owner, name, description, private, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![owner, name, description, private, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn delete_repo(&self, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM repos WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn repo(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        self.c()
            .query_row(
                &format!("SELECT {REPO_COLS} FROM repos WHERE owner = ?1 AND name = ?2"),
                [owner, name],
                repo_row,
            )
            .optional()
    }

    /// Repos `viewer` can see, optionally for one owner.
    pub fn visible_repos(&self, viewer: Option<&User>, owner: Option<&str>) -> Result<Vec<Repo>> {
        let (uid, uname, admin) = viewer.map_or((-1, "", false), |u| (u.id, u.name.as_str(), u.is_admin));
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {REPO_COLS} FROM repos r
             WHERE (?1 IS NULL OR r.owner = ?1)
               AND (r.private = 0 OR ?2 OR r.owner = ?3
                    OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?4))
             ORDER BY r.owner, r.name"
        ))?;
        st.query_map(params![owner, admin, uname, uid], repo_row)?.collect()
    }

    pub fn access(&self, user: Option<&User>, repo: &Repo) -> Result<Access> {
        let base = if repo.private { Access::None } else { Access::Read };
        let Some(u) = user else { return Ok(base) };
        if u.is_admin || u.name.eq_ignore_ascii_case(&repo.owner) {
            return Ok(Access::Admin);
        }
        let role: Option<i64> = self
            .c()
            .query_row(
                "SELECT role FROM collaborators WHERE repo_id = ?1 AND user_id = ?2",
                params![repo.id, u.id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(role.map(Access::from_role).unwrap_or(Access::None).max(base))
    }

    /// May `user` create repos under `owner`? (Orgs come later.)
    pub fn can_create_under(&self, user: &User, owner: &str) -> bool {
        user.is_admin || user.name.eq_ignore_ascii_case(owner)
    }
}
