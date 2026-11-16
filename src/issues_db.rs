//! Issues (and, later, PRs): numbering, comments, labels, milestones,
//! assignees and the timeline. Extends `Db`.

use rusqlite::{params, OptionalExtension, Row};

use crate::db::{now, Db, Result};

#[derive(Clone)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub description: String,
}

pub struct Milestone {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub due: Option<i64>,
    pub closed: bool,
    pub open_issues: i64,
    pub closed_issues: i64,
}

pub struct Issue {
    pub id: i64,
    pub number: i64,
    pub is_pr: bool,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub author_id: Option<i64>,
    pub closed: bool,
    pub milestone_id: Option<i64>,
    pub milestone: Option<String>,
    pub comments: i64,
    pub created: i64,
    pub updated: i64,
    pub closed_at: Option<i64>,
    pub labels: Vec<Label>,
}

pub struct Comment {
    pub id: i64,
    pub issue_id: i64,
    pub author: Option<String>,
    pub author_id: Option<i64>,
    pub body: String,
    pub created: i64,
    pub updated: i64,
}

pub struct Event {
    pub actor: Option<String>,
    pub kind: String,
    pub data: String,
    pub created: i64,
}

#[derive(Default)]
pub struct Filter {
    pub closed: bool,
    pub label: Option<String>,
    pub milestone: Option<String>,
    pub assignee: Option<String>,
    pub author: Option<String>,
}

/// `#rrggbb` only: colors go into a style attribute.
pub fn valid_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

const ISSUE_COLS: &str = "i.id, i.number, i.is_pr, i.title, i.body, u.name, i.author_id, i.closed, i.milestone_id,
     m.title, i.comments, i.created, i.updated, i.closed_at";
const ISSUE_FROM: &str = "issues i LEFT JOIN users u ON u.id = i.author_id LEFT JOIN milestones m ON m.id = i.milestone_id";

fn issue_row(r: &Row<'_>) -> Result<Issue> {
    Ok(Issue {
        id: r.get(0)?,
        number: r.get(1)?,
        is_pr: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        author: r.get(5)?,
        author_id: r.get(6)?,
        closed: r.get(7)?,
        milestone_id: r.get(8)?,
        milestone: r.get(9)?,
        comments: r.get(10)?,
        created: r.get(11)?,
        updated: r.get(12)?,
        closed_at: r.get(13)?,
        labels: Vec::new(),
    })
}

fn label_row(r: &Row<'_>) -> Result<Label> {
    Ok(Label { id: r.get(0)?, name: r.get(1)?, color: r.get(2)?, description: r.get(3)? })
}

/// SQL fragment + params for the non-state parts of a filter.
fn filter_sql(f: &Filter) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut args = Vec::new();
    if let Some(l) = &f.label {
        args.push(l.clone());
        sql += &format!(
            " AND EXISTS (SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id
                          WHERE il.issue_id = i.id AND l.name = ?{})",
            args.len() + 2
        );
    }
    if let Some(m) = &f.milestone {
        args.push(m.clone());
        sql += &format!(" AND m.title = ?{}", args.len() + 2);
    }
    if let Some(a) = &f.assignee {
        args.push(a.clone());
        sql += &format!(
            " AND EXISTS (SELECT 1 FROM issue_assignees ia JOIN users au ON au.id = ia.user_id
                          WHERE ia.issue_id = i.id AND au.name = ?{})",
            args.len() + 2
        );
    }
    if let Some(a) = &f.author {
        args.push(a.clone());
        sql += &format!(" AND u.name = ?{}", args.len() + 2);
    }
    (sql, args)
}

impl Db {
    /// Allocates the next per-repo number (shared by issues and PRs).
    pub fn create_issue(&self, repo_id: i64, author_id: i64, title: &str, body: &str, is_pr: bool) -> Result<(i64, i64)> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let number: i64 = tx.query_row(
            "UPDATE repos SET next_number = next_number + 1 WHERE id = ?1 RETURNING next_number - 1",
            [repo_id],
            |r| r.get(0),
        )?;
        let t = now();
        tx.execute(
            "INSERT INTO issues (repo_id, number, is_pr, title, body, author_id, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![repo_id, number, is_pr, title, body, author_id, t],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok((id, number))
    }

    fn attach_labels(&self, issues: &mut [Issue]) -> Result<()> {
        if issues.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = issues.iter().map(|i| i.id.to_string()).collect();
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT il.issue_id, l.id, l.name, l.color, l.description FROM issue_labels il
             JOIN labels l ON l.id = il.label_id WHERE il.issue_id IN ({}) ORDER BY l.name",
            ids.join(",")
        ))?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, Label { id: r.get(1)?, name: r.get(2)?, color: r.get(3)?, description: r.get(4)? }))
        })?;
        for row in rows {
            let (iid, label) = row?;
            if let Some(i) = issues.iter_mut().find(|i| i.id == iid) {
                i.labels.push(label);
            }
        }
        Ok(())
    }

    pub fn issue(&self, repo_id: i64, number: i64) -> Result<Option<Issue>> {
        let issue = self
            .c()
            .query_row(
                &format!("SELECT {ISSUE_COLS} FROM {ISSUE_FROM} WHERE i.repo_id = ?1 AND i.number = ?2"),
                params![repo_id, number],
                issue_row,
            )
            .optional()?;
        match issue {
            Some(i) => {
                let mut v = [i];
                self.attach_labels(&mut v)?;
                let [i] = v;
                Ok(Some(i))
            }
            None => Ok(None),
        }
    }

    /// (title, closed, is_pr) — for reference links.
    pub fn issue_brief(&self, repo_id: i64, number: i64) -> Result<Option<(String, bool, bool)>> {
        self.c()
            .query_row(
                "SELECT title, closed, is_pr FROM issues WHERE repo_id = ?1 AND number = ?2",
                params![repo_id, number],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    pub fn issue_id(&self, repo_id: i64, number: i64) -> Result<Option<i64>> {
        self.c()
            .query_row("SELECT id FROM issues WHERE repo_id = ?1 AND number = ?2", params![repo_id, number], |r| r.get(0))
            .optional()
    }

    pub fn list_issues(&self, repo_id: i64, is_pr: bool, f: &Filter, limit: usize, offset: usize) -> Result<Vec<Issue>> {
        let (extra, args) = filter_sql(f);
        let mut issues = {
            let c = self.c();
            let mut st = c.prepare(&format!(
                "SELECT {ISSUE_COLS} FROM {ISSUE_FROM}
                 WHERE i.repo_id = ?1 AND i.is_pr = ?2 AND i.closed = {}{extra}
                 ORDER BY i.number DESC LIMIT {limit} OFFSET {offset}",
                f.closed as i64
            ))?;
            let mut p: Vec<&dyn rusqlite::ToSql> = vec![&repo_id, &is_pr];
            p.extend(args.iter().map(|a| a as &dyn rusqlite::ToSql));
            st.query_map(p.as_slice(), issue_row)?.collect::<Result<Vec<_>>>()?
        };
        self.attach_labels(&mut issues)?;
        Ok(issues)
    }

    /// (open, closed) under the same non-state filter.
    pub fn issue_counts(&self, repo_id: i64, is_pr: bool, f: &Filter) -> Result<(i64, i64)> {
        let (extra, args) = filter_sql(f);
        let c = self.c();
        let mut p: Vec<&dyn rusqlite::ToSql> = vec![&repo_id, &is_pr];
        p.extend(args.iter().map(|a| a as &dyn rusqlite::ToSql));
        c.query_row(
            &format!(
                "SELECT COALESCE(SUM(i.closed = 0), 0), COALESCE(SUM(i.closed = 1), 0) FROM {ISSUE_FROM}
                 WHERE i.repo_id = ?1 AND i.is_pr = ?2{extra}"
            ),
            p.as_slice(),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    pub fn open_count(&self, repo_id: i64, is_pr: bool) -> Result<i64> {
        self.c().query_row(
            "SELECT COUNT(*) FROM issues WHERE repo_id = ?1 AND is_pr = ?2 AND closed = 0",
            params![repo_id, is_pr],
            |r| r.get(0),
        )
    }

    pub fn edit_issue(&self, issue: &Issue, actor: i64, title: &str, body: &str) -> Result<()> {
        let c = self.c();
        c.execute("UPDATE issues SET title = ?1, body = ?2, updated = ?3 WHERE id = ?4", params![title, body, now(), issue.id])?;
        drop(c);
        if title != issue.title {
            self.add_event(issue.id, Some(actor), "renamed", &format!("{}\t{}", issue.title, title), None)?;
        }
        Ok(())
    }

    /// Returns false if already in that state.
    pub fn set_issue_closed(&self, issue_id: i64, closed: bool, actor: Option<i64>, kind: &str, data: &str, ref_key: Option<&str>) -> Result<bool> {
        let t = now();
        let changed = self.c().execute(
            "UPDATE issues SET closed = ?1, closed_at = CASE WHEN ?1 THEN ?2 ELSE NULL END, updated = ?2
             WHERE id = ?3 AND closed != ?1",
            params![closed, t, issue_id],
        )? > 0;
        if changed {
            self.add_event(issue_id, actor, kind, data, ref_key)?;
        }
        Ok(changed)
    }

    // ---- comments ----

    pub fn add_comment(&self, issue_id: i64, author_id: i64, body: &str) -> Result<i64> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let t = now();
        tx.execute(
            "INSERT INTO comments (issue_id, author_id, body, created, updated) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![issue_id, author_id, body, t],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("UPDATE issues SET comments = comments + 1, updated = ?1 WHERE id = ?2", params![t, issue_id])?;
        tx.commit()?;
        Ok(id)
    }

    pub fn comment(&self, id: i64) -> Result<Option<Comment>> {
        self.c()
            .query_row(
                "SELECT c.id, c.issue_id, u.name, c.author_id, c.body, c.created, c.updated
                 FROM comments c LEFT JOIN users u ON u.id = c.author_id WHERE c.id = ?1",
                [id],
                |r| {
                    Ok(Comment {
                        id: r.get(0)?,
                        issue_id: r.get(1)?,
                        author: r.get(2)?,
                        author_id: r.get(3)?,
                        body: r.get(4)?,
                        created: r.get(5)?,
                        updated: r.get(6)?,
                    })
                },
            )
            .optional()
    }

    pub fn comments(&self, issue_id: i64) -> Result<Vec<Comment>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT c.id, c.issue_id, u.name, c.author_id, c.body, c.created, c.updated
             FROM comments c LEFT JOIN users u ON u.id = c.author_id WHERE c.issue_id = ?1 ORDER BY c.id",
        )?;
        st.query_map([issue_id], |r| {
            Ok(Comment {
                id: r.get(0)?,
                issue_id: r.get(1)?,
                author: r.get(2)?,
                author_id: r.get(3)?,
                body: r.get(4)?,
                created: r.get(5)?,
                updated: r.get(6)?,
            })
        })?
        .collect()
    }

    pub fn edit_comment(&self, id: i64, body: &str) -> Result<()> {
        self.c().execute("UPDATE comments SET body = ?1, updated = ?2 WHERE id = ?3", params![body, now(), id])?;
        Ok(())
    }

    pub fn delete_comment(&self, id: i64, issue_id: i64) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        if tx.execute("DELETE FROM comments WHERE id = ?1", [id])? > 0 {
            tx.execute("UPDATE issues SET comments = comments - 1 WHERE id = ?1", [issue_id])?;
        }
        tx.commit()
    }

    // ---- events ----

    /// `ref_key` dedupes events that may be produced more than once (e.g. the
    /// same commit pushed to two branches). Returns false if deduped.
    pub fn add_event(&self, issue_id: i64, actor: Option<i64>, kind: &str, data: &str, ref_key: Option<&str>) -> Result<bool> {
        Ok(self.c().execute(
            "INSERT OR IGNORE INTO issue_events (issue_id, actor_id, kind, data, created, ref_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![issue_id, actor, kind, data, now(), ref_key],
        )? > 0)
    }

    pub fn events(&self, issue_id: i64) -> Result<Vec<Event>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name, e.kind, e.data, e.created FROM issue_events e
             LEFT JOIN users u ON u.id = e.actor_id WHERE e.issue_id = ?1 ORDER BY e.id",
        )?;
        st.query_map([issue_id], |r| Ok(Event { actor: r.get(0)?, kind: r.get(1)?, data: r.get(2)?, created: r.get(3)? }))?
            .collect()
    }

    // ---- labels ----

    pub fn labels(&self, repo_id: i64) -> Result<Vec<Label>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, name, color, description FROM labels WHERE repo_id = ?1 ORDER BY name")?;
        st.query_map([repo_id], label_row)?.collect()
    }

    /// Label name -> open issue count, for the labels page.
    pub fn label_open_counts(&self, repo_id: i64) -> Result<Vec<(i64, i64)>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT il.label_id, COUNT(*) FROM issue_labels il JOIN issues i ON i.id = il.issue_id
             WHERE i.repo_id = ?1 AND i.closed = 0 GROUP BY il.label_id",
        )?;
        st.query_map([repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    pub fn create_label(&self, repo_id: i64, name: &str, color: &str, description: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO labels (repo_id, name, color, description) VALUES (?1, ?2, ?3, ?4)",
            params![repo_id, name, color, description],
        )?;
        Ok(())
    }

    pub fn update_label(&self, repo_id: i64, id: i64, name: &str, color: &str, description: &str) -> Result<()> {
        self.c().execute(
            "UPDATE labels SET name = ?1, color = ?2, description = ?3 WHERE id = ?4 AND repo_id = ?5",
            params![name, color, description, id, repo_id],
        )?;
        Ok(())
    }

    pub fn delete_label(&self, repo_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM labels WHERE id = ?1 AND repo_id = ?2", params![id, repo_id])?;
        Ok(())
    }

    /// Replace an issue's labels with `ids` (only labels of `repo_id` count); logs events.
    pub fn set_issue_labels(&self, repo_id: i64, issue_id: i64, ids: &[i64], actor: i64) -> Result<()> {
        let all = self.labels(repo_id)?;
        let current: Vec<i64> = {
            let c = self.c();
            let mut st = c.prepare("SELECT label_id FROM issue_labels WHERE issue_id = ?1")?;
            st.query_map([issue_id], |r| r.get(0))?.collect::<Result<_>>()?
        };
        for l in &all {
            let want = ids.contains(&l.id);
            let have = current.contains(&l.id);
            if want && !have {
                self.c().execute("INSERT INTO issue_labels (issue_id, label_id) VALUES (?1, ?2)", params![issue_id, l.id])?;
                self.add_event(issue_id, Some(actor), "labeled", &l.name, None)?;
            } else if !want && have {
                self.c().execute("DELETE FROM issue_labels WHERE issue_id = ?1 AND label_id = ?2", params![issue_id, l.id])?;
                self.add_event(issue_id, Some(actor), "unlabeled", &l.name, None)?;
            }
        }
        Ok(())
    }

    // ---- milestones ----

    pub fn milestones(&self, repo_id: i64) -> Result<Vec<Milestone>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT m.id, m.title, m.description, m.due, m.closed,
                    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.closed = 0),
                    (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.closed = 1)
             FROM milestones m WHERE m.repo_id = ?1 ORDER BY m.closed, m.due IS NULL, m.due, m.title",
        )?;
        st.query_map([repo_id], |r| {
            Ok(Milestone {
                id: r.get(0)?,
                title: r.get(1)?,
                description: r.get(2)?,
                due: r.get(3)?,
                closed: r.get(4)?,
                open_issues: r.get(5)?,
                closed_issues: r.get(6)?,
            })
        })?
        .collect()
    }

    pub fn create_milestone(&self, repo_id: i64, title: &str, description: &str, due: Option<i64>) -> Result<()> {
        self.c().execute(
            "INSERT INTO milestones (repo_id, title, description, due, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo_id, title, description, due, now()],
        )?;
        Ok(())
    }

    pub fn update_milestone(&self, repo_id: i64, id: i64, title: &str, description: &str, due: Option<i64>, closed: bool) -> Result<()> {
        self.c().execute(
            "UPDATE milestones SET title = ?1, description = ?2, due = ?3, closed = ?4 WHERE id = ?5 AND repo_id = ?6",
            params![title, description, due, closed, id, repo_id],
        )?;
        Ok(())
    }

    pub fn delete_milestone(&self, repo_id: i64, id: i64) -> Result<()> {
        self.c().execute("DELETE FROM milestones WHERE id = ?1 AND repo_id = ?2", params![id, repo_id])?;
        Ok(())
    }

    pub fn set_issue_milestone(&self, repo_id: i64, issue: &Issue, milestone: Option<i64>, actor: i64) -> Result<()> {
        if issue.milestone_id == milestone {
            return Ok(());
        }
        let title: Option<String> = match milestone {
            Some(id) => Some(
                self.c()
                    .query_row("SELECT title FROM milestones WHERE id = ?1 AND repo_id = ?2", params![id, repo_id], |r| r.get(0))
                    .optional()?
                    .ok_or(rusqlite::Error::QueryReturnedNoRows)?,
            ),
            None => None,
        };
        self.c().execute("UPDATE issues SET milestone_id = ?1 WHERE id = ?2", params![milestone, issue.id])?;
        match title {
            Some(t) => self.add_event(issue.id, Some(actor), "milestoned", &t, None)?,
            None => self.add_event(issue.id, Some(actor), "demilestoned", issue.milestone.as_deref().unwrap_or(""), None)?,
        };
        Ok(())
    }

    // ---- assignees ----

    pub fn assignees(&self, issue_id: i64) -> Result<Vec<String>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT u.name FROM issue_assignees a JOIN users u ON u.id = a.user_id WHERE a.issue_id = ?1 ORDER BY u.name",
        )?;
        st.query_map([issue_id], |r| r.get(0))?.collect()
    }

    pub fn add_assignee(&self, issue_id: i64, user_id: i64, name: &str, actor: i64) -> Result<()> {
        if self.c().execute("INSERT OR IGNORE INTO issue_assignees (issue_id, user_id) VALUES (?1, ?2)", params![issue_id, user_id])? > 0 {
            self.add_event(issue_id, Some(actor), "assigned", name, None)?;
        }
        Ok(())
    }

    pub fn remove_assignee(&self, issue_id: i64, name: &str, actor: i64) -> Result<()> {
        let n = self.c().execute(
            "DELETE FROM issue_assignees WHERE issue_id = ?1 AND user_id = (SELECT id FROM users WHERE name = ?2)",
            params![issue_id, name],
        )?;
        if n > 0 {
            self.add_event(issue_id, Some(actor), "unassigned", name, None)?;
        }
        Ok(())
    }
}
