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

/// v2: orgs (users with is_org), forks, archive, stars, branch protection.
const SCHEMA_V2: &str = "
ALTER TABLE users ADD COLUMN is_org INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN description TEXT NOT NULL DEFAULT '';
CREATE TABLE org_members (
    org_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role INTEGER NOT NULL,
    PRIMARY KEY (org_id, user_id)
);
ALTER TABLE repos ADD COLUMN fork_of INTEGER REFERENCES repos(id) ON DELETE SET NULL;
ALTER TABLE repos ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
CREATE TABLE stars (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    created INTEGER NOT NULL,
    PRIMARY KEY (user_id, repo_id)
);
CREATE INDEX stars_repo ON stars(repo_id);
CREATE TABLE protected_branches (
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    allow_force_push INTEGER NOT NULL DEFAULT 0,
    allow_delete INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repo_id, branch)
);
";

/// v3: issues (shared numbering with PRs), comments, labels, milestones,
/// assignees, timeline events. Existing repos get the default labels.
const SCHEMA_V3: &str = "
ALTER TABLE repos ADD COLUMN next_number INTEGER NOT NULL DEFAULT 1;
CREATE TABLE milestones (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    due INTEGER,
    closed INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    UNIQUE (repo_id, title)
);
CREATE TABLE issues (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    is_pr INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    closed INTEGER NOT NULL DEFAULT 0,
    milestone_id INTEGER REFERENCES milestones(id) ON DELETE SET NULL,
    comments INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    closed_at INTEGER,
    UNIQUE (repo_id, number)
);
CREATE INDEX issues_list ON issues(repo_id, is_pr, closed, number DESC);
CREATE TABLE comments (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX comments_issue ON comments(issue_id, id);
CREATE TABLE labels (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name TEXT NOT NULL COLLATE NOCASE,
    color TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    UNIQUE (repo_id, name)
);
CREATE TABLE issue_labels (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);
CREATE INDEX issue_labels_label ON issue_labels(label_id);
CREATE TABLE issue_assignees (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, user_id)
);
CREATE INDEX issue_assignees_user ON issue_assignees(user_id);
CREATE TABLE issue_events (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    data TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    ref_key TEXT UNIQUE
);
CREATE INDEX issue_events_issue ON issue_events(issue_id, id);
INSERT INTO labels (repo_id, name, color, description)
    SELECT r.id, d.name, d.color, d.description FROM repos r CROSS JOIN (
        SELECT 'bug' AS name, '#d73a4a' AS color, 'Something is broken' AS description
        UNION ALL SELECT 'enhancement', '#84b6eb', 'New feature or request'
        UNION ALL SELECT 'question', '#cc317c', 'Needs more information'
        UNION ALL SELECT 'documentation', '#0075ca', 'Docs only'
        UNION ALL SELECT 'good first issue', '#7057ff', 'Good for newcomers'
        UNION ALL SELECT 'duplicate', '#cfd3d7', 'Already reported'
        UNION ALL SELECT 'wontfix', '#e4e669', 'Will not be worked on'
    ) d;
";

/// v4: pull requests (an issue row with is_pr = 1 plus this), revisions per
/// push, reviews, inline review comments, "require PR" protection.
const SCHEMA_V4: &str = "
CREATE TABLE pulls (
    issue_id INTEGER PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    head_repo_id INTEGER REFERENCES repos(id) ON DELETE SET NULL,
    head_branch TEXT NOT NULL,
    base_branch TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    merged INTEGER NOT NULL DEFAULT 0,
    merged_at INTEGER,
    merged_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
    merge_sha TEXT
);
CREATE INDEX pulls_head ON pulls(head_repo_id, head_branch);
CREATE TABLE pull_revisions (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    n INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    base_sha TEXT NOT NULL,
    forced INTEGER NOT NULL DEFAULT 0,
    pusher_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created INTEGER NOT NULL,
    UNIQUE (issue_id, n)
);
CREATE TABLE reviews (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    state TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    head_sha TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE INDEX reviews_issue ON reviews(issue_id);
CREATE TABLE review_comments (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    path TEXT NOT NULL,
    side TEXT NOT NULL,
    line INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    body TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE INDEX review_comments_issue ON review_comments(issue_id);
ALTER TABLE protected_branches ADD COLUMN require_pr INTEGER NOT NULL DEFAULT 0;
";

/// v5: webhooks + delivery queue, commit statuses, required checks,
/// watches, notifications, activity, full-text search over issues/comments.
const SCHEMA_V5: &str = "
CREATE TABLE webhooks (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    secret TEXT NOT NULL DEFAULT '',
    events TEXT NOT NULL DEFAULT '*',
    active INTEGER NOT NULL DEFAULT 1,
    created INTEGER NOT NULL
);
CREATE TABLE hook_deliveries (
    id INTEGER PRIMARY KEY,
    hook_id INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event TEXT NOT NULL,
    payload TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_try INTEGER NOT NULL,
    status INTEGER,
    response TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER,
    done INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE INDEX hook_deliveries_due ON hook_deliveries(done, next_try);
CREATE INDEX hook_deliveries_hook ON hook_deliveries(hook_id, id DESC);
CREATE TABLE statuses (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    sha TEXT NOT NULL,
    context TEXT NOT NULL,
    state TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    target_url TEXT NOT NULL DEFAULT '',
    creator_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created INTEGER NOT NULL
);
CREATE INDEX statuses_sha ON statuses(repo_id, sha, context, id);
ALTER TABLE protected_branches ADD COLUMN required_checks TEXT NOT NULL DEFAULT '';
CREATE TABLE watches (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, repo_id)
);
CREATE INDEX watches_repo ON watches(repo_id);
INSERT OR IGNORE INTO watches (user_id, repo_id)
    SELECT u.id, r.id FROM repos r JOIN users u ON u.name = r.owner AND u.is_org = 0;
CREATE TABLE notifications (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    unread INTEGER NOT NULL DEFAULT 1,
    updated INTEGER NOT NULL,
    UNIQUE (user_id, issue_id)
);
CREATE INDEX notifications_user ON notifications(user_id, unread, updated DESC);
CREATE TABLE activity (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    actor_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    data TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL
);
CREATE INDEX activity_repo ON activity(repo_id, id DESC);
CREATE VIRTUAL TABLE issues_fts USING fts5(title, body, content='issues', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER issues_fts_ai AFTER INSERT ON issues BEGIN
    INSERT INTO issues_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER issues_fts_ad AFTER DELETE ON issues BEGIN
    INSERT INTO issues_fts(issues_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
END;
CREATE TRIGGER issues_fts_au AFTER UPDATE OF title, body ON issues BEGIN
    INSERT INTO issues_fts(issues_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
    INSERT INTO issues_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;
INSERT INTO issues_fts(issues_fts) VALUES ('rebuild');
CREATE VIRTUAL TABLE comments_fts USING fts5(body, content='comments', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER comments_fts_ai AFTER INSERT ON comments BEGIN
    INSERT INTO comments_fts(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER comments_fts_ad AFTER DELETE ON comments BEGIN
    INSERT INTO comments_fts(comments_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER comments_fts_au AFTER UPDATE OF body ON comments BEGIN
    INSERT INTO comments_fts(comments_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO comments_fts(rowid, body) VALUES (new.id, new.body);
END;
INSERT INTO comments_fts(comments_fts) VALUES ('rebuild');
";

/// v6: releases + assets, pull mirrors, TOTP two-factor, pending sessions.
const SCHEMA_V6: &str = "
ALTER TABLE users ADD COLUMN totp_secret TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN totp_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN pending INTEGER NOT NULL DEFAULT 0;
ALTER TABLE repos ADD COLUMN mirror INTEGER NOT NULL DEFAULT 0;
CREATE TABLE releases (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    author_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created INTEGER NOT NULL
);
CREATE INDEX releases_repo ON releases(repo_id, id DESC);
CREATE TABLE release_assets (
    id INTEGER PRIMARY KEY,
    release_id INTEGER NOT NULL REFERENCES releases(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    downloads INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE mirrors (
    repo_id INTEGER PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    interval_hours INTEGER NOT NULL DEFAULT 6,
    last_run INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT ''
);
";

/// v7: draft/prerelease releases, email notifications, opt-in last-commit
/// badges, org teams with per-repo grants.
const SCHEMA_V7: &str = "
ALTER TABLE releases ADD COLUMN draft INTEGER NOT NULL DEFAULT 0;
ALTER TABLE releases ADD COLUMN prerelease INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN notify_email INTEGER NOT NULL DEFAULT 0;
ALTER TABLE repos ADD COLUMN show_last_commit INTEGER NOT NULL DEFAULT 0;
CREATE TABLE teams (
    id INTEGER PRIMARY KEY,
    org_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    UNIQUE (org_id, name)
);
CREATE TABLE team_members (
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role INTEGER NOT NULL DEFAULT 2,
    PRIMARY KEY (team_id, user_id)
);
CREATE INDEX team_members_user ON team_members(user_id);
CREATE TABLE team_repos (
    team_id INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    role INTEGER NOT NULL DEFAULT 2,
    PRIMARY KEY (team_id, repo_id)
);
CREATE INDEX team_repos_repo ON team_repos(repo_id);
CREATE TABLE mail_outbox (
    id INTEGER PRIMARY KEY,
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    created INTEGER NOT NULL,
    sent INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT ''
);
CREATE INDEX mail_due ON mail_outbox(sent, id);
";

pub const DEFAULT_LABELS: &[(&str, &str, &str)] = &[
    ("bug", "#d73a4a", "Something is broken"),
    ("enhancement", "#84b6eb", "New feature or request"),
    ("question", "#cc317c", "Needs more information"),
    ("documentation", "#0075ca", "Docs only"),
    ("good first issue", "#7057ff", "Good for newcomers"),
    ("duplicate", "#cfd3d7", "Already reported"),
    ("wontfix", "#e4e669", "Will not be worked on"),
];

pub const SESSION_DAYS: i64 = 30;

/// Names that would collide with top-level routes.
const RESERVED: &[&str] = &["static", "login", "logout", "settings", "new", "admin", "api", "explore", "search", "notifications"];

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
    /// Unread notifications (filled in for the signed-in viewer only).
    pub unread: i64,
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
    pub archived: bool,
    pub fork_of: Option<i64>,
    /// Pull mirror: looms fetches refs from `mirrors.url`; pushes are refused.
    pub mirror: bool,
    /// Opt-in: tree view shows the last commit touching each entry.
    pub show_last_commit: bool,
}

pub struct Collaborator {
    pub name: String,
    pub access: Access,
}

pub struct Member {
    pub name: String,
    pub access: Access,
}

pub struct Protection {
    pub branch: String,
    pub allow_force_push: bool,
    pub allow_delete: bool,
    pub require_pr: bool,
    /// Status contexts that must be "success" on a PR's head before merging.
    pub required_checks: Vec<String>,
}

/// A user or an org (they share one namespace).
pub struct Owner {
    pub id: i64,
    pub name: String,
    pub is_org: bool,
    pub description: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Access {
    None = 0,
    Read = 1,
    Write = 2,
    Admin = 3,
}

impl Access {
    pub fn from_role(r: i64) -> Self {
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
    Ok(User { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, is_admin: r.get(3)?, unread: 0 })
}

fn repo_row(r: &rusqlite::Row<'_>) -> Result<Repo> {
    Ok(Repo {
        id: r.get(0)?,
        owner: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        private: r.get(4)?,
        archived: r.get(5)?,
        fork_of: r.get(6)?,
        mirror: r.get(7)?,
        show_last_commit: r.get(8)?,
    })
}

const USER_COLS: &str = "id, name, email, is_admin";
const REPO_COLS: &str = "id, owner, name, description, private, archived, fork_of, mirror, show_last_commit";

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
        if v < 2 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V2} PRAGMA user_version = 2; COMMIT;"))?;
        }
        if v < 3 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V3} PRAGMA user_version = 3; COMMIT;"))?;
        }
        if v < 4 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V4} PRAGMA user_version = 4; COMMIT;"))?;
        }
        if v < 5 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V5} PRAGMA user_version = 5; COMMIT;"))?;
        }
        if v < 6 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V6} PRAGMA user_version = 6; COMMIT;"))?;
        }
        if v < 7 {
            c.execute_batch(&format!("BEGIN; {SCHEMA_V7} PRAGMA user_version = 7; COMMIT;"))?;
        }
        Ok(Db(Mutex::new(c)))
    }

    pub(crate) fn c(&self) -> MutexGuard<'_, Connection> {
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
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE name = ?1 AND is_org = 0"), [name], user_row)
            .optional()
    }

    /// Consistent snapshot of the database file (SQLite VACUUM INTO).
    pub fn backup_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        self.c().execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        Ok(())
    }

    pub fn schema_version(&self) -> i64 {
        self.c().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        self.c()
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1 AND is_org = 0"), [id], user_row)
            .optional()
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users WHERE is_org = 0 ORDER BY name"))?;
        st.query_map([], user_row)?.collect()
    }

    pub fn delete_user(&self, name: &str) -> Result<bool> {
        Ok(self.c().execute("DELETE FROM users WHERE name = ?1 AND is_org = 0", [name])? > 0)
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
            .query_row(&format!("SELECT {USER_COLS}, password FROM users WHERE name = ?1 AND is_org = 0"), [name], |r| {
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

    /// Replace the TOTP secret and enabled flag.
    pub fn set_totp(&self, user_id: i64, secret: &str, enabled: bool) -> Result<()> {
        self.c().execute("UPDATE users SET totp_secret = ?1, totp_enabled = ?2 WHERE id = ?3", params![secret, enabled, user_id])?;
        Ok(())
    }

    /// (secret, enabled) for a user.
    pub fn totp(&self, user_id: i64) -> Result<(String, bool)> {
        self.c().query_row("SELECT totp_secret, totp_enabled FROM users WHERE id = ?1", [user_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
    }

    // ---- mirrors ----

    pub fn mirror(&self, repo_id: i64) -> Result<Option<(String, i64, i64, String, String)>> {
        use rusqlite::OptionalExtension;
        self.c()
            .query_row("SELECT url, interval_hours, last_run, status, message FROM mirrors WHERE repo_id = ?1", [repo_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()
    }

    pub fn set_mirror_config(&self, repo_id: i64, url: &str, hours: i64) -> Result<()> {
        self.c().execute(
            "INSERT INTO mirrors (repo_id, url, interval_hours) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo_id) DO UPDATE SET url = excluded.url, interval_hours = excluded.interval_hours",
            params![repo_id, url, hours],
        )?;
        Ok(())
    }

    pub fn remove_mirror(&self, repo_id: i64) -> Result<()> {
        self.c().execute("DELETE FROM mirrors WHERE repo_id = ?1", [repo_id])?;
        Ok(())
    }

    pub fn mark_mirror_run(&self, repo_id: i64, status: &str, message: &str) -> Result<()> {
        self.c().execute(
            "UPDATE mirrors SET last_run = ?1, status = ?2, message = ?3 WHERE repo_id = ?4",
            params![now(), status, message, repo_id],
        )?;
        Ok(())
    }

    /// (repo_id, url, interval_hours) for mirrors whose interval has passed.
    pub fn due_mirrors(&self) -> Result<Vec<(i64, String, i64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT repo_id, url, interval_hours FROM mirrors
             WHERE last_run + interval_hours * 3600 <= ?1 OR (last_run = 0 AND status = '')",
        )?;
        st.query_map([now()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    }

    // ---- sessions ----

    /// Returns the raw cookie value; only its hash is stored.
    pub fn create_session(&self, user_id: i64) -> Result<String> {
        self.create_session_inner(user_id, false, SESSION_DAYS * 86400)
    }

    /// A session that exists only to carry a login through the 2FA step.
    pub fn create_pending_session(&self, user_id: i64) -> Result<String> {
        self.create_session_inner(user_id, true, 300)
    }

    fn create_session_inner(&self, user_id: i64, pending: bool, ttl: i64) -> Result<String> {
        let raw = random_hex(32);
        let c = self.c();
        c.execute("DELETE FROM sessions WHERE expires < ?1", [now()])?;
        c.execute(
            "INSERT INTO sessions (hash, user_id, expires, pending) VALUES (?1, ?2, ?3, ?4)",
            params![sha256_hex(&raw), user_id, now() + ttl, pending],
        )?;
        Ok(raw)
    }

    /// Turn a pending session into a real one.
    pub fn confirm_session(&self, raw: &str) -> Result<bool> {
        Ok(self.c().execute(
            "UPDATE sessions SET pending = 0, expires = ?1 WHERE hash = ?2 AND pending = 1",
            params![now() + SESSION_DAYS * 86400, sha256_hex(raw)],
        )? > 0)
    }

    /// User behind a pending session (2FA step only).
    pub fn pending_session_user(&self, raw: &str) -> Result<Option<User>> {
        self.c()
            .query_row(
                "SELECT u.id, u.name, u.email, u.is_admin FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.hash = ?1 AND s.expires > ?2 AND s.pending = 1",
                params![sha256_hex(raw), now()],
                user_row,
            )
            .optional()
    }

    pub fn session_user(&self, raw: &str) -> Result<Option<User>> {
        self.c()
            .query_row(
                "SELECT u.id, u.name, u.email, u.is_admin FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.hash = ?1 AND s.expires > ?2 AND s.pending = 0",
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
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO repos (owner, name, description, private, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![owner, name, description, private, now()],
        )?;
        let id = tx.last_insert_rowid();
        for (n, color, d) in DEFAULT_LABELS {
            tx.execute("INSERT INTO labels (repo_id, name, color, description) VALUES (?1, ?2, ?3, ?4)", params![id, n, color, d])?;
        }
        tx.commit()?;
        Ok(id)
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
                    OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?4)
                    OR EXISTS (SELECT 1 FROM org_members m JOIN users o ON o.id = m.org_id
                               WHERE o.name = r.owner AND m.user_id = ?4)
                    OR EXISTS (SELECT 1 FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id
                               WHERE tr.repo_id = r.id AND tm.user_id = ?4))
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
        // Highest of: collaborator role, org membership role.
        let role: Option<i64> = self
            .c()
            .query_row(
                "SELECT MAX(role) FROM (
                    SELECT role FROM collaborators WHERE repo_id = ?1 AND user_id = ?2
                    UNION ALL
                    SELECT m.role FROM org_members m JOIN users o ON o.id = m.org_id
                    WHERE o.name = ?3 AND o.is_org = 1 AND m.user_id = ?2
                    UNION ALL
                    SELECT tr.role FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id
                    WHERE tr.repo_id = ?1 AND tm.user_id = ?2)",
                params![repo.id, u.id, repo.owner],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(role.map(Access::from_role).unwrap_or(Access::None).max(base))
    }

    /// May `user` create repos under `owner`? Themselves, or an org where they have write.
    pub fn can_create_under(&self, user: &User, owner: &str) -> bool {
        user.is_admin
            || user.name.eq_ignore_ascii_case(owner)
            || self.org_role(owner, user.id).is_ok_and(|r| r >= Access::Write)
    }

    /// Owners `user` can create repos under: themselves first, then orgs.
    pub fn creatable_owners(&self, user: &User) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT o.name FROM org_members m JOIN users o ON o.id = m.org_id
             WHERE m.user_id = ?1 AND m.role >= 2 ORDER BY o.name",
        )?;
        let mut v = vec![user.name.clone()];
        v.extend(st.query_map([user.id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>>>()?);
        Ok(v)
    }

    pub fn update_repo(&self, id: i64, description: &str, private: bool, archived: bool, show_last_commit: bool) -> Result<()> {
        self.c().execute(
            "UPDATE repos SET description = ?1, private = ?2, archived = ?3, show_last_commit = ?4 WHERE id = ?5",
            params![description, private, archived, show_last_commit, id],
        )?;
        Ok(())
    }

    pub fn move_repo(&self, id: i64, owner: &str, name: &str) -> Result<()> {
        self.c().execute("UPDATE repos SET owner = ?1, name = ?2 WHERE id = ?3", params![owner, name, id])?;
        Ok(())
    }

    pub fn set_mirror(&self, id: i64, mirror: bool) -> Result<()> {
        self.c().execute("UPDATE repos SET mirror = ?1 WHERE id = ?2", params![mirror, id])?;
        Ok(())
    }

    pub fn set_fork_of(&self, id: i64, parent: i64) -> Result<()> {
        self.c().execute("UPDATE repos SET fork_of = ?1 WHERE id = ?2", params![parent, id])?;
        Ok(())
    }

    pub fn repo_by_id(&self, id: i64) -> Result<Option<Repo>> {
        self.c()
            .query_row(&format!("SELECT {REPO_COLS} FROM repos WHERE id = ?1"), [id], repo_row)
            .optional()
    }

    pub fn fork_count(&self, id: i64) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM repos WHERE fork_of = ?1", [id], |r| r.get(0))
    }

    // ---- collaborators ----

    pub fn collaborators(&self, repo_id: i64) -> Result<Vec<Collaborator>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name, c.role FROM collaborators c JOIN users u ON u.id = c.user_id
             WHERE c.repo_id = ?1 ORDER BY u.name",
        )?;
        st.query_map([repo_id], |r| Ok(Collaborator { name: r.get(0)?, access: Access::from_role(r.get(1)?) }))?
            .collect()
    }

    pub fn set_collaborator(&self, repo_id: i64, user_id: i64, access: Access) -> Result<()> {
        self.c().execute(
            "INSERT INTO collaborators (repo_id, user_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo_id, user_id) DO UPDATE SET role = excluded.role",
            params![repo_id, user_id, access as i64],
        )?;
        Ok(())
    }

    pub fn remove_collaborator(&self, repo_id: i64, name: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM collaborators WHERE repo_id = ?1 AND user_id = (SELECT id FROM users WHERE name = ?2)",
            params![repo_id, name],
        )?;
        Ok(())
    }

    // ---- stars ----

    pub fn star_count(&self, repo_id: i64) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM stars WHERE repo_id = ?1", [repo_id], |r| r.get(0))
    }

    pub fn starred(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        self.c().query_row(
            "SELECT EXISTS (SELECT 1 FROM stars WHERE user_id = ?1 AND repo_id = ?2)",
            params![user_id, repo_id],
            |r| r.get(0),
        )
    }

    /// Toggle; returns the new state.
    pub fn toggle_star(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        let c = self.c();
        if c.execute("DELETE FROM stars WHERE user_id = ?1 AND repo_id = ?2", params![user_id, repo_id])? > 0 {
            return Ok(false);
        }
        c.execute("INSERT INTO stars (user_id, repo_id, created) VALUES (?1, ?2, ?3)", params![user_id, repo_id, now()])?;
        Ok(true)
    }

    // ---- branch protection ----

    pub fn protections(&self, repo_id: i64) -> Result<Vec<Protection>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT branch, allow_force_push, allow_delete, require_pr, required_checks FROM protected_branches
             WHERE repo_id = ?1 ORDER BY branch",
        )?;
        st.query_map([repo_id], |r| {
            let checks: String = r.get(4)?;
            Ok(Protection {
                branch: r.get(0)?,
                allow_force_push: r.get(1)?,
                allow_delete: r.get(2)?,
                require_pr: r.get(3)?,
                required_checks: checks.split(',').map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned).collect(),
            })
        })?
        .collect()
    }

    pub fn set_protection(&self, repo_id: i64, p: &Protection) -> Result<()> {
        self.c().execute(
            "INSERT INTO protected_branches (repo_id, branch, allow_force_push, allow_delete, require_pr, required_checks)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (repo_id, branch) DO UPDATE SET allow_force_push = excluded.allow_force_push,
                                                         allow_delete = excluded.allow_delete,
                                                         require_pr = excluded.require_pr,
                                                         required_checks = excluded.required_checks",
            params![repo_id, p.branch, p.allow_force_push, p.allow_delete, p.require_pr, p.required_checks.join(",")],
        )?;
        Ok(())
    }

    pub fn remove_protection(&self, repo_id: i64, branch: &str) -> Result<()> {
        self.c().execute("DELETE FROM protected_branches WHERE repo_id = ?1 AND branch = ?2", params![repo_id, branch])?;
        Ok(())
    }

    // ---- owners & orgs ----

    pub fn owner(&self, name: &str) -> Result<Option<Owner>> {
        self.c()
            .query_row("SELECT id, name, is_org, description FROM users WHERE name = ?1", [name], |r| {
                Ok(Owner { id: r.get(0)?, name: r.get(1)?, is_org: r.get(2)?, description: r.get(3)? })
            })
            .optional()
    }

    /// Creates the org with `creator` as admin.
    pub fn create_org(&self, name: &str, description: &str, creator: i64) -> Result<i64> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO users (name, description, is_org, created) VALUES (?1, ?2, 1, ?3)",
            params![name, description, now()],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, 3)", params![id, creator])?;
        tx.commit()?;
        Ok(id)
    }

    pub fn update_org(&self, id: i64, description: &str) -> Result<()> {
        self.c().execute("UPDATE users SET description = ?1 WHERE id = ?2 AND is_org = 1", params![description, id])?;
        Ok(())
    }

    pub fn delete_org(&self, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM users WHERE id = ?1 AND is_org = 1", [id])?;
        Ok(())
    }

    pub fn org_role(&self, org: &str, user_id: i64) -> Result<Access> {
        let r: Option<i64> = self
            .c()
            .query_row(
                "SELECT m.role FROM org_members m JOIN users o ON o.id = m.org_id
                 WHERE o.name = ?1 AND o.is_org = 1 AND m.user_id = ?2",
                params![org, user_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(r.map(Access::from_role).unwrap_or(Access::None))
    }

    pub fn members(&self, org_id: i64) -> Result<Vec<Member>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name, m.role FROM org_members m JOIN users u ON u.id = m.user_id
             WHERE m.org_id = ?1 ORDER BY m.role DESC, u.name",
        )?;
        st.query_map([org_id], |r| Ok(Member { name: r.get(0)?, access: Access::from_role(r.get(1)?) }))?
            .collect()
    }

    pub fn set_member(&self, org_id: i64, user_id: i64, access: Access) -> Result<()> {
        self.c().execute(
            "INSERT INTO org_members (org_id, user_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = excluded.role",
            params![org_id, user_id, access as i64],
        )?;
        Ok(())
    }

    pub fn remove_member(&self, org_id: i64, name: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM org_members WHERE org_id = ?1 AND user_id = (SELECT id FROM users WHERE name = ?2)",
            params![org_id, name],
        )?;
        Ok(())
    }

    pub fn admin_count(&self, org_id: i64) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM org_members WHERE org_id = ?1 AND role = 3", [org_id], |r| r.get(0))
    }

    /// Orgs `user_id` belongs to.
    pub fn orgs_of(&self, user_id: i64) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT o.name FROM org_members m JOIN users o ON o.id = m.org_id WHERE m.user_id = ?1 ORDER BY o.name",
        )?;
        st.query_map([user_id], |r| r.get(0))?.collect()
    }

    pub fn repo_count_total(&self) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
    }

    // ---- teams ----

    pub fn teams(&self, org_id: i64) -> Result<Vec<(i64, String, String)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, name, description FROM teams WHERE org_id = ?1 ORDER BY name")?;
        st.query_map([org_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    }

    pub fn create_team(&self, org_id: i64, name: &str, description: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO teams (org_id, name, description, created) VALUES (?1, ?2, ?3, ?4)",
            params![org_id, name, description, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn delete_team(&self, org_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM teams WHERE id = ?1 AND org_id = ?2", params![id, org_id])?;
        Ok(())
    }

    pub fn team_members(&self, team_id: i64) -> Result<Vec<Member>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name, tm.role FROM team_members tm JOIN users u ON u.id = tm.user_id
             WHERE tm.team_id = ?1 ORDER BY u.name",
        )?;
        st.query_map([team_id], |r| Ok(Member { name: r.get(0)?, access: Access::from_role(r.get(1)?) }))?.collect()
    }

    pub fn set_team_member(&self, team_id: i64, user_id: i64, access: Access) -> Result<()> {
        self.c().execute(
            "INSERT INTO team_members (team_id, user_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (team_id, user_id) DO UPDATE SET role = excluded.role",
            params![team_id, user_id, access as i64],
        )?;
        Ok(())
    }

    pub fn remove_team_member(&self, team_id: i64, name: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM team_members WHERE team_id = ?1 AND user_id = (SELECT id FROM users WHERE name = ?2)",
            params![team_id, name],
        )?;
        Ok(())
    }

    /// (repo full name, role) granted to a team.
    pub fn team_repos(&self, team_id: i64) -> Result<Vec<(String, Access)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT r.owner || '/' || r.name, tr.role FROM team_repos tr JOIN repos r ON r.id = tr.repo_id
             WHERE tr.team_id = ?1 ORDER BY 1",
        )?;
        st.query_map([team_id], |r| Ok((r.get(0)?, Access::from_role(r.get(1)?))))?.collect()
    }

    pub fn grant_team_repo(&self, team_id: i64, repo_id: i64, access: Access) -> Result<()> {
        self.c().execute(
            "INSERT INTO team_repos (team_id, repo_id, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (team_id, repo_id) DO UPDATE SET role = excluded.role",
            params![team_id, repo_id, access as i64],
        )?;
        Ok(())
    }

    pub fn revoke_team_repo(&self, team_id: i64, full: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM team_repos WHERE team_id = ?1 AND repo_id = (SELECT id FROM repos WHERE owner || '/' || name = ?2)",
            params![team_id, full],
        )?;
        Ok(())
    }

    /// Teams (name, role) with access to a repo.
    pub fn repo_teams(&self, repo_id: i64) -> Result<Vec<(String, Access)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT t.name, tr.role FROM team_repos tr JOIN teams t ON t.id = tr.team_id WHERE tr.repo_id = ?1 ORDER BY t.name",
        )?;
        st.query_map([repo_id], |r| Ok((r.get(0)?, Access::from_role(r.get(1)?))))?.collect()
    }

    // ---- email notifications ----

    pub fn set_notify_email(&self, user_id: i64, email: &str, enabled: bool) -> Result<()> {
        self.c().execute("UPDATE users SET email = ?1, notify_email = ?2 WHERE id = ?3", params![email, enabled, user_id])?;
        Ok(())
    }

    pub fn notify_prefs(&self, user_id: i64) -> Result<(String, bool)> {
        self.c().query_row("SELECT email, notify_email FROM users WHERE id = ?1", [user_id], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    pub fn queue_mail(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO mail_outbox (to_addr, subject, body, created) VALUES (?1, ?2, ?3, ?4)",
            params![to, subject, body, now()],
        )?;
        Ok(())
    }

    pub fn has_due_mail(&self) -> bool {
        self.c().query_row("SELECT EXISTS (SELECT 1 FROM mail_outbox WHERE sent = 0)", [], |r| r.get(0)).unwrap_or(false)
    }

    /// Unsent mail, oldest first.
    pub fn due_mail(&self, limit: i64) -> Result<Vec<(i64, String, String, String)>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, to_addr, subject, body FROM mail_outbox WHERE sent = 0 ORDER BY id LIMIT ?1")?;
        st.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect()
    }

    pub fn mark_mail(&self, id: i64, ok: bool, error: &str) -> Result<()> {
        self.c().execute("UPDATE mail_outbox SET sent = ?1, error = ?2 WHERE id = ?3", params![ok, error, id])?;
        Ok(())
    }


    pub fn repo_count(&self, owner: &str) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM repos WHERE owner = ?1", [owner], |r| r.get(0))
    }
}
