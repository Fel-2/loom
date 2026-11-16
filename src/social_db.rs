//! Integrations storage: webhooks + delivery queue, commit statuses,
//! watches, notifications, activity, full-text search.

use rusqlite::{params, OptionalExtension};

use crate::db::{now, Db, Repo, Result, User};

pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub secret: String,
    pub events: String,
    pub active: bool,
}

impl Webhook {
    pub fn wants(&self, event: &str) -> bool {
        self.active && (self.events == "*" || self.events.split(',').any(|e| e.trim() == event))
    }
}

pub struct Delivery {
    pub id: i64,
    pub event: String,
    pub payload: String,
    pub attempts: i64,
    pub status: Option<i64>,
    pub response: String,
    pub duration_ms: Option<i64>,
    pub done: bool,
    pub created: i64,
}

#[derive(Clone)]
pub struct Status {
    pub context: String,
    pub state: String,
    pub description: String,
    pub target_url: String,
    pub creator: Option<String>,
    pub created: i64,
}

pub struct Notification {
    pub repo: String,
    pub number: i64,
    pub is_pr: bool,
    pub title: String,
    pub closed: bool,
    pub reason: String,
    pub actor: Option<String>,
    pub unread: bool,
    pub updated: i64,
}

pub struct Activity {
    pub repo: String,
    pub actor: Option<String>,
    pub kind: String,
    pub data: String,
    pub created: i64,
}

pub struct SearchHit {
    pub repo: String,
    pub number: i64,
    pub is_pr: bool,
    pub title: String,
    pub closed: bool,
    pub snippet: String,
}

pub const STATES: &[&str] = &["pending", "success", "failure", "error"];
pub const HOOK_EVENTS: &[&str] = &["push", "issues", "issue_comment", "pull_request", "pull_request_review", "status"];
const KEEP_DELIVERIES: i64 = 50;

/// Turn user input into a safe FTS5 query: every term quoted, AND-ed.
pub fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .take(10)
        .map(|t| t.chars().filter(|c| *c != '"').collect::<String>())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// SQL predicate: repo `r` is visible to the viewer (?v_id, ?v_name, ?v_admin).
const VISIBLE: &str = "(r.private = 0 OR :admin OR r.owner = :uname
    OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = :uid)
    OR EXISTS (SELECT 1 FROM org_members m JOIN users o ON o.id = m.org_id WHERE o.name = r.owner AND m.user_id = :uid))";

fn viewer_params(v: Option<&User>) -> (i64, String, bool) {
    v.map_or((-1, String::new(), false), |u| (u.id, u.name.clone(), u.is_admin))
}

impl Db {
    // ---- webhooks ----

    pub fn webhooks(&self, repo_id: i64) -> Result<Vec<Webhook>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, url, secret, events, active FROM webhooks WHERE repo_id = ?1 ORDER BY id")?;
        st.query_map([repo_id], |r| Ok(Webhook { id: r.get(0)?, url: r.get(1)?, secret: r.get(2)?, events: r.get(3)?, active: r.get(4)? }))?
        .collect()
    }

    pub fn webhook(&self, repo_id: i64, id: i64) -> Result<Option<Webhook>> {
        self.c()
            .query_row(
                "SELECT id, url, secret, events, active FROM webhooks WHERE repo_id = ?1 AND id = ?2",
                params![repo_id, id],
                |r| Ok(Webhook { id: r.get(0)?, url: r.get(1)?, secret: r.get(2)?, events: r.get(3)?, active: r.get(4)? }),
            )
            .optional()
    }

    pub fn create_webhook(&self, repo_id: i64, url: &str, secret: &str, events: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO webhooks (repo_id, url, secret, events, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo_id, url, secret, events, now()],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn delete_webhook(&self, repo_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM webhooks WHERE repo_id = ?1 AND id = ?2", params![repo_id, id])?;
        Ok(())
    }

    pub fn queue_delivery(&self, hook_id: i64, event: &str, payload: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO hook_deliveries (hook_id, event, payload, next_try, created) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![hook_id, event, payload, now()],
        )?;
        let id = c.last_insert_rowid();
        // Bounded log per hook.
        c.execute(
            "DELETE FROM hook_deliveries WHERE hook_id = ?1 AND done = 1 AND id NOT IN
             (SELECT id FROM hook_deliveries WHERE hook_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![hook_id, KEEP_DELIVERIES],
        )?;
        Ok(id)
    }

    /// Anything to deliver right now? (Cheap: indexed, runs on the async thread.)
    pub fn has_due_deliveries(&self) -> bool {
        self.c()
            .query_row("SELECT EXISTS (SELECT 1 FROM hook_deliveries WHERE done = 0 AND next_try <= ?1)", [now()], |r| r.get(0))
            .unwrap_or(false)
    }

    /// Due deliveries with their hook's url/secret.
    pub fn due_deliveries(&self, limit: i64) -> Result<Vec<(Delivery, String, String)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT d.id, d.event, d.payload, d.attempts, d.status, d.response, d.duration_ms, d.done, d.created, w.url, w.secret
             FROM hook_deliveries d JOIN webhooks w ON w.id = d.hook_id
             WHERE d.done = 0 AND d.next_try <= ?1 ORDER BY d.next_try LIMIT ?2",
        )?;
        st.query_map(params![now(), limit], |r| {
            Ok((
                Delivery {
                    id: r.get(0)?,
                    event: r.get(1)?,
                    payload: r.get(2)?,
                    attempts: r.get(3)?,
                    status: r.get(4)?,
                    response: r.get(5)?,
                    duration_ms: r.get(6)?,
                    done: r.get(7)?,
                    created: r.get(8)?,
                },
                r.get(9)?,
                r.get(10)?,
            ))
        })?
        .collect()
    }

    pub fn record_attempt(&self, id: i64, status: Option<i64>, response: &str, ms: i64, done: bool, next_try: i64) -> Result<()> {
        self.c().execute(
            "UPDATE hook_deliveries SET attempts = attempts + 1, status = ?1, response = ?2, duration_ms = ?3, done = ?4, next_try = ?5
             WHERE id = ?6",
            params![status, response, ms, done, next_try, id],
        )?;
        Ok(())
    }

    pub fn deliveries(&self, hook_id: i64) -> Result<Vec<Delivery>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, event, payload, attempts, status, response, duration_ms, done, created
             FROM hook_deliveries WHERE hook_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        st.query_map(params![hook_id, KEEP_DELIVERIES], |r| {
            Ok(Delivery {
                id: r.get(0)?,
                event: r.get(1)?,
                payload: r.get(2)?,
                attempts: r.get(3)?,
                status: r.get(4)?,
                response: r.get(5)?,
                duration_ms: r.get(6)?,
                done: r.get(7)?,
                created: r.get(8)?,
            })
        })?
        .collect()
    }

    /// Re-queue a past delivery (same payload) as a new one.
    pub fn redeliver(&self, hook_id: i64, delivery_id: i64) -> Result<bool> {
        let row: Option<(String, String)> = self
            .c()
            .query_row("SELECT event, payload FROM hook_deliveries WHERE id = ?1 AND hook_id = ?2", params![delivery_id, hook_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match row {
            Some((e, p)) => {
                self.queue_delivery(hook_id, &e, &p)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn last_delivery_status(&self, hook_id: i64) -> Result<Option<(Option<i64>, bool)>> {
        self.c()
            .query_row("SELECT status, done FROM hook_deliveries WHERE hook_id = ?1 ORDER BY id DESC LIMIT 1", [hook_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
    }

    // ---- statuses ----

    pub fn add_status(&self, repo_id: i64, sha: &str, s: &Status, creator: i64) -> Result<()> {
        self.c().execute(
            "INSERT INTO statuses (repo_id, sha, context, state, description, target_url, creator_id, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![repo_id, sha, s.context, s.state, s.description, s.target_url, creator, now()],
        )?;
        Ok(())
    }

    /// Latest status per context for a commit.
    pub fn statuses(&self, repo_id: i64, sha: &str) -> Result<Vec<Status>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT s.context, s.state, s.description, s.target_url, u.name, s.created FROM statuses s
             LEFT JOIN users u ON u.id = s.creator_id
             WHERE s.id IN (SELECT MAX(id) FROM statuses WHERE repo_id = ?1 AND sha = ?2 GROUP BY context)
             ORDER BY s.context",
        )?;
        st.query_map(params![repo_id, sha], |r| {
            Ok(Status { context: r.get(0)?, state: r.get(1)?, description: r.get(2)?, target_url: r.get(3)?, creator: r.get(4)?, created: r.get(5)? })
        })?
        .collect()
    }

    // ---- watches ----

    pub fn watch(&self, user_id: i64, repo_id: i64) -> Result<()> {
        self.c().execute("INSERT OR IGNORE INTO watches (user_id, repo_id) VALUES (?1, ?2)", params![user_id, repo_id])?;
        Ok(())
    }

    pub fn toggle_watch(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        let c = self.c();
        if c.execute("DELETE FROM watches WHERE user_id = ?1 AND repo_id = ?2", params![user_id, repo_id])? > 0 {
            return Ok(false);
        }
        c.execute("INSERT INTO watches (user_id, repo_id) VALUES (?1, ?2)", params![user_id, repo_id])?;
        Ok(true)
    }

    pub fn watching(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        self.c().query_row(
            "SELECT EXISTS (SELECT 1 FROM watches WHERE user_id = ?1 AND repo_id = ?2)",
            params![user_id, repo_id],
            |r| r.get(0),
        )
    }

    pub fn watchers(&self, repo_id: i64) -> Result<Vec<User>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.id, u.name, u.email, u.is_admin FROM watches w JOIN users u ON u.id = w.user_id WHERE w.repo_id = ?1",
        )?;
        st.query_map([repo_id], |r| Ok(User { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, is_admin: r.get(3)?, unread: 0 }))?
            .collect()
    }

    /// Everyone who took part in an issue: author, commenters, assignees, reviewers.
    pub fn participants(&self, issue_id: i64) -> Result<Vec<(User, &'static str)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.id, u.name, u.email, u.is_admin, MIN(x.prio) FROM (
                SELECT author_id AS uid, 2 AS prio FROM issues WHERE id = ?1
                UNION ALL SELECT user_id, 1 FROM issue_assignees WHERE issue_id = ?1
                UNION ALL SELECT author_id, 3 FROM comments WHERE issue_id = ?1
                UNION ALL SELECT author_id, 3 FROM reviews WHERE issue_id = ?1
                UNION ALL SELECT author_id, 3 FROM review_comments WHERE issue_id = ?1
             ) x JOIN users u ON u.id = x.uid GROUP BY u.id",
        )?;
        st.query_map([issue_id], |r| {
            let reason = match r.get::<_, i64>(4)? {
                1 => "assigned",
                2 => "author",
                _ => "participating",
            };
            Ok((User { id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, is_admin: r.get(3)?, unread: 0 }, reason))
        })?
        .collect()
    }

    // ---- notifications ----

    pub fn notify(&self, user_id: i64, repo_id: i64, issue_id: i64, reason: &str, actor: Option<i64>) -> Result<()> {
        self.c().execute(
            "INSERT INTO notifications (user_id, repo_id, issue_id, reason, actor_id, unread, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
             ON CONFLICT (user_id, issue_id) DO UPDATE SET reason = excluded.reason, actor_id = excluded.actor_id,
                 unread = 1, updated = excluded.updated",
            params![user_id, repo_id, issue_id, reason, actor, now()],
        )?;
        Ok(())
    }

    pub fn unread_count(&self, user_id: i64) -> Result<i64> {
        self.c().query_row("SELECT COUNT(*) FROM notifications WHERE user_id = ?1 AND unread = 1", [user_id], |r| r.get(0))
    }

    pub fn notifications(&self, user_id: i64, all: bool) -> Result<Vec<Notification>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT r.owner || '/' || r.name, i.number, i.is_pr, i.title, i.closed, n.reason, a.name, n.unread, n.updated
             FROM notifications n JOIN issues i ON i.id = n.issue_id JOIN repos r ON r.id = n.repo_id
             LEFT JOIN users a ON a.id = n.actor_id
             WHERE n.user_id = ?1 {} ORDER BY n.updated DESC LIMIT 100",
            if all { "" } else { "AND n.unread = 1" }
        ))?;
        st.query_map([user_id], |r| {
            Ok(Notification {
                repo: r.get(0)?,
                number: r.get(1)?,
                is_pr: r.get(2)?,
                title: r.get(3)?,
                closed: r.get(4)?,
                reason: r.get(5)?,
                actor: r.get(6)?,
                unread: r.get(7)?,
                updated: r.get(8)?,
            })
        })?
        .collect()
    }

    pub fn mark_read(&self, user_id: i64, issue_id: i64) -> Result<()> {
        self.c().execute("UPDATE notifications SET unread = 0 WHERE user_id = ?1 AND issue_id = ?2 AND unread = 1", params![user_id, issue_id])?;
        Ok(())
    }

    pub fn mark_all_read(&self, user_id: i64) -> Result<()> {
        self.c().execute("UPDATE notifications SET unread = 0 WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    // ---- activity ----

    pub fn add_activity(&self, repo_id: i64, actor: Option<i64>, kind: &str, data: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO activity (repo_id, actor_id, kind, data, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo_id, actor, kind, data, now()],
        )?;
        Ok(())
    }

    /// Recent activity in repos the viewer can see; `watched_by` limits to a
    /// user's watched repos (dashboard).
    pub fn activity(&self, viewer: Option<&User>, watched_by: Option<i64>, owner: Option<&str>, limit: i64) -> Result<Vec<Activity>> {
        let (uid, uname, admin) = viewer_params(viewer);
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT r.owner || '/' || r.name, u.name, a.kind, a.data, a.created FROM activity a
             JOIN repos r ON r.id = a.repo_id LEFT JOIN users u ON u.id = a.actor_id
             WHERE {VISIBLE}
               AND (:watched IS NULL OR EXISTS (SELECT 1 FROM watches w WHERE w.repo_id = r.id AND w.user_id = :watched))
               AND (:owner IS NULL OR r.owner = :owner OR u.name = :owner)
             ORDER BY a.id DESC LIMIT :limit"
        ))?;
        st.query_map(
            rusqlite::named_params! { ":uid": uid, ":uname": uname, ":admin": admin, ":watched": watched_by, ":owner": owner, ":limit": limit },
            |r| Ok(Activity { repo: r.get(0)?, actor: r.get(1)?, kind: r.get(2)?, data: r.get(3)?, created: r.get(4)? }),
        )?
        .collect()
    }

    // ---- search ----

    /// Issues/PRs matching `q` in title, body or comments, in visible repos.
    pub fn search_issues(&self, viewer: Option<&User>, q: &str, repo_id: Option<i64>, limit: i64) -> Result<Vec<SearchHit>> {
        let Some(fq) = fts_query(q) else { return Ok(Vec::new()) };
        let (uid, uname, admin) = viewer_params(viewer);
        let c = self.c();
        let mut st = c.prepare(&format!(
            "WITH hits AS (
                SELECT rowid AS iid, snippet(issues_fts, 1, '[', ']', '…', 12) AS snip, bm25(issues_fts) AS score
                FROM issues_fts WHERE issues_fts MATCH :q
                UNION ALL
                SELECT cm.issue_id, snippet(comments_fts, 0, '[', ']', '…', 12), bm25(comments_fts) + 1.0
                FROM comments_fts JOIN comments cm ON cm.id = comments_fts.rowid WHERE comments_fts MATCH :q
             ), best AS (SELECT iid, snip, MIN(score) AS score FROM hits GROUP BY iid)
             SELECT r.owner || '/' || r.name, i.number, i.is_pr, i.title, i.closed, b.snip
             FROM best b JOIN issues i ON i.id = b.iid JOIN repos r ON r.id = i.repo_id
             WHERE {VISIBLE} AND (:repo IS NULL OR r.id = :repo)
             ORDER BY b.score LIMIT :limit"
        ))?;
        st.query_map(
            rusqlite::named_params! { ":q": fq, ":uid": uid, ":uname": uname, ":admin": admin, ":repo": repo_id, ":limit": limit },
            |r| Ok(SearchHit { repo: r.get(0)?, number: r.get(1)?, is_pr: r.get(2)?, title: r.get(3)?, closed: r.get(4)?, snippet: r.get(5)? }),
        )?
        .collect()
    }

    pub fn search_repos(&self, viewer: Option<&User>, q: &str, limit: i64) -> Result<Vec<Repo>> {
        let all = self.visible_repos(viewer, None)?;
        let terms: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        Ok(all
            .into_iter()
            .filter(|r| {
                let hay = format!("{}/{} {}", r.owner, r.name, r.description).to_lowercase();
                terms.iter().all(|t| hay.contains(t))
            })
            .take(limit as usize)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fts_query_is_quoted() {
        assert_eq!(super::fts_query(r#"foo "bar OR* baz"#).unwrap(), r#""foo" "bar" "OR*" "baz""#);
        assert!(super::fts_query("   ").is_none());
    }
}
