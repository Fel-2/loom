//! Pull request storage: head/base, revisions, reviews, inline comments.
//! A PR is an `issues` row with `is_pr = 1` plus a `pulls` row.

use rusqlite::{params, OptionalExtension};

use crate::db::{now, Db, Result};

#[derive(Clone)]
pub struct Pull {
    pub issue_id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub head_repo_id: Option<i64>,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub merged: bool,
    pub merged_at: Option<i64>,
    pub merged_by: Option<String>,
    pub merge_sha: Option<String>,
    pub closed: bool,
    pub draft: bool,
}

pub struct Revision {
    pub n: i64,
    pub head_sha: String,
    pub base_sha: String,
    pub forced: bool,
    pub pusher: Option<String>,
    pub created: i64,
}

pub struct Review {
    pub author: Option<String>,
    pub state: String,
    pub body: String,
    pub created: i64,
}

pub struct ReviewComment {
    pub id: i64,
    pub author: Option<String>,
    pub author_id: Option<i64>,
    pub path: String,
    pub side: String,
    pub line: i64,
    pub head_sha: String,
    pub body: String,
    pub created: i64,
}

const PULL_COLS: &str = "p.issue_id, i.repo_id, i.number, p.head_repo_id, p.head_branch, p.base_branch, p.head_sha,
     p.merged, p.merged_at, u.name, p.merge_sha, i.closed, p.draft";
const PULL_FROM: &str = "pulls p JOIN issues i ON i.id = p.issue_id LEFT JOIN users u ON u.id = p.merged_by";

fn pull_row(r: &rusqlite::Row<'_>) -> Result<Pull> {
    Ok(Pull {
        issue_id: r.get(0)?,
        repo_id: r.get(1)?,
        number: r.get(2)?,
        head_repo_id: r.get(3)?,
        head_branch: r.get(4)?,
        base_branch: r.get(5)?,
        head_sha: r.get(6)?,
        merged: r.get(7)?,
        merged_at: r.get(8)?,
        merged_by: r.get(9)?,
        merge_sha: r.get(10)?,
        closed: r.get(11)?,
        draft: r.get(12)?,
    })
}

impl Db {
    #[allow(clippy::too_many_arguments)]
    pub fn create_pull(
        &self,
        repo_id: i64,
        author_id: i64,
        title: &str,
        body: &str,
        head_repo_id: i64,
        head_branch: &str,
        base_branch: &str,
        head_sha: &str,
        base_sha: &str,
        draft: bool,
    ) -> Result<(i64, i64)> {
        let (issue_id, number) = self.create_issue(repo_id, author_id, title, body, true)?;
        let c = self.c();
        c.execute(
            "INSERT INTO pulls (issue_id, head_repo_id, head_branch, base_branch, head_sha, draft) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![issue_id, head_repo_id, head_branch, base_branch, head_sha, draft],
        )?;
        c.execute(
            "INSERT INTO pull_revisions (issue_id, n, head_sha, base_sha, pusher_id, created) VALUES (?1, 1, ?2, ?3, ?4, ?5)",
            params![issue_id, head_sha, base_sha, author_id, now()],
        )?;
        Ok((issue_id, number))
    }

    /// Draft -> ready (or back again, for the author).
    pub fn set_pull_draft(&self, issue_id: i64, draft: bool) -> Result<()> {
        self.c().execute("UPDATE pulls SET draft = ?1 WHERE issue_id = ?2 AND merged = 0", params![draft, issue_id])?;
        Ok(())
    }

    pub fn pull(&self, repo_id: i64, number: i64) -> Result<Option<Pull>> {
        self.c()
            .query_row(
                &format!("SELECT {PULL_COLS} FROM {PULL_FROM} WHERE i.repo_id = ?1 AND i.number = ?2"),
                params![repo_id, number],
                pull_row,
            )
            .optional()
    }

    /// An open PR already proposing this head into this base?
    pub fn open_pull_for(&self, repo_id: i64, head_repo_id: i64, head_branch: &str, base_branch: &str) -> Result<Option<i64>> {
        self.c()
            .query_row(
                "SELECT i.number FROM pulls p JOIN issues i ON i.id = p.issue_id
                 WHERE i.repo_id = ?1 AND p.head_repo_id = ?2 AND p.head_branch = ?3 AND p.base_branch = ?4 AND i.closed = 0",
                params![repo_id, head_repo_id, head_branch, base_branch],
                |r| r.get(0),
            )
            .optional()
    }

    /// Open PRs whose head is `branch` in `head_repo_id`.
    pub fn open_pulls_from(&self, head_repo_id: i64, branch: &str) -> Result<Vec<Pull>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {PULL_COLS} FROM {PULL_FROM} WHERE p.head_repo_id = ?1 AND p.head_branch = ?2 AND i.closed = 0"
        ))?;
        st.query_map(params![head_repo_id, branch], pull_row)?.collect()
    }

    /// Open PRs targeting `branch` in `repo_id`.
    pub fn open_pulls_into(&self, repo_id: i64, branch: &str) -> Result<Vec<Pull>> {
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT {PULL_COLS} FROM {PULL_FROM} WHERE i.repo_id = ?1 AND p.base_branch = ?2 AND i.closed = 0"
        ))?;
        st.query_map(params![repo_id, branch], pull_row)?.collect()
    }

    /// Record a new head; returns the revision number.
    pub fn add_revision(&self, issue_id: i64, head_sha: &str, base_sha: &str, forced: bool, pusher: Option<i64>) -> Result<i64> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let n: i64 = tx.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM pull_revisions WHERE issue_id = ?1", [issue_id], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO pull_revisions (issue_id, n, head_sha, base_sha, forced, pusher_id, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![issue_id, n, head_sha, base_sha, forced, pusher, now()],
        )?;
        tx.execute("UPDATE pulls SET head_sha = ?1 WHERE issue_id = ?2", params![head_sha, issue_id])?;
        tx.execute("UPDATE issues SET updated = ?1 WHERE id = ?2", params![now(), issue_id])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn revisions(&self, issue_id: i64) -> Result<Vec<Revision>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT r.n, r.head_sha, r.base_sha, r.forced, u.name, r.created FROM pull_revisions r
             LEFT JOIN users u ON u.id = r.pusher_id WHERE r.issue_id = ?1 ORDER BY r.n",
        )?;
        st.query_map([issue_id], |r| {
            Ok(Revision { n: r.get(0)?, head_sha: r.get(1)?, base_sha: r.get(2)?, forced: r.get(3)?, pusher: r.get(4)?, created: r.get(5)? })
        })?
        .collect()
    }

    /// Marks merged + closed. Returns false if it was already closed/merged.
    pub fn mark_merged(&self, issue_id: i64, by: Option<i64>, merge_sha: &str) -> Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let t = now();
        let n = tx.execute(
            "UPDATE pulls SET merged = 1, merged_at = ?1, merged_by = ?2, merge_sha = ?3 WHERE issue_id = ?4 AND merged = 0",
            params![t, by, merge_sha, issue_id],
        )?;
        if n > 0 {
            tx.execute("UPDATE issues SET closed = 1, closed_at = ?1, updated = ?1 WHERE id = ?2", params![t, issue_id])?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    // ---- reviews ----

    pub fn add_review(&self, issue_id: i64, author_id: i64, state: &str, body: &str, head_sha: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO reviews (issue_id, author_id, state, body, head_sha, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![issue_id, author_id, state, body, head_sha, now()],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE issues SET updated = ?1 WHERE id = ?2", params![now(), issue_id])?;
        Ok(id)
    }

    pub fn reviews(&self, issue_id: i64) -> Result<Vec<Review>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name, r.state, r.body, r.created FROM reviews r
             LEFT JOIN users u ON u.id = r.author_id WHERE r.issue_id = ?1 ORDER BY r.id",
        )?;
        st.query_map([issue_id], |r| Ok(Review { author: r.get(0)?, state: r.get(1)?, body: r.get(2)?, created: r.get(3)? }))?
        .collect()
    }

    // ---- inline comments ----

    #[allow(clippy::too_many_arguments)]
    pub fn add_review_comment(&self, issue_id: i64, author_id: i64, path: &str, side: &str, line: i64, head_sha: &str, body: &str) -> Result<i64> {
        let c = self.c();
        c.execute(
            "INSERT INTO review_comments (issue_id, author_id, path, side, line, head_sha, body, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![issue_id, author_id, path, side, line, head_sha, body, now()],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE issues SET updated = ?1 WHERE id = ?2", params![now(), issue_id])?;
        Ok(id)
    }

    pub fn review_comments(&self, issue_id: i64) -> Result<Vec<ReviewComment>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT rc.id, u.name, rc.author_id, rc.path, rc.side, rc.line, rc.head_sha, rc.body, rc.created
             FROM review_comments rc LEFT JOIN users u ON u.id = rc.author_id
             WHERE rc.issue_id = ?1 ORDER BY rc.id",
        )?;
        st.query_map([issue_id], |r| {
            Ok(ReviewComment {
                id: r.get(0)?,
                author: r.get(1)?,
                author_id: r.get(2)?,
                path: r.get(3)?,
                side: r.get(4)?,
                line: r.get(5)?,
                head_sha: r.get(6)?,
                body: r.get(7)?,
                created: r.get(8)?,
            })
        })?
        .collect()
    }

    pub fn delete_review_comment(&self, issue_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM review_comments WHERE id = ?1 AND issue_id = ?2", params![id, issue_id])?;
        Ok(())
    }

    /// Direct forks of `repo_id` (candidate PR heads).
    pub fn forks_of(&self, repo_id: i64) -> Result<Vec<crate::db::Repo>> {
        let ids: Vec<i64> = {
            let c = self.c();
            let mut st = c.prepare("SELECT id FROM repos WHERE fork_of = ?1 ORDER BY owner, name")?;
            st.query_map([repo_id], |r| r.get(0))?.collect::<Result<_>>()?
        };
        Ok(ids.into_iter().filter_map(|id| self.repo_by_id(id).ok().flatten()).collect())
    }

    pub fn review_comment_author(&self, issue_id: i64, id: i64) -> Result<Option<Option<i64>>> {
        self.c()
            .query_row("SELECT author_id FROM review_comments WHERE id = ?1 AND issue_id = ?2", params![id, issue_id], |r| r.get(0))
            .optional()
    }
}
