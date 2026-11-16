//! One place where things that happened fan out: activity log, webhook
//! queue, notifications. Called from web handlers and from the post-receive
//! hook process (which only has a Db, so delivery is done by the server).

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::db::{Access, Db, Repo, User};
use crate::issues_db::Issue;
use crate::pulls_db::Pull;
use crate::social_db::Status;

pub struct Bus<'a> {
    pub db: &'a Db,
    /// Public base URL for links in payloads.
    pub url: &'a str,
}

pub enum Ev<'a> {
    Push { repo: &'a Repo, pusher: Option<&'a User>, refname: &'a str, before: &'a str, after: &'a str, commits: Vec<(String, String)> },
    Issue { repo: &'a Repo, actor: &'a User, issue: &'a Issue, action: &'a str },
    Comment { repo: &'a Repo, actor: &'a User, issue: &'a Issue, body: &'a str },
    Pull { repo: &'a Repo, actor: Option<&'a User>, issue: &'a Issue, pull: &'a Pull, action: &'a str },
    Review { repo: &'a Repo, actor: &'a User, issue: &'a Issue, pull: &'a Pull, state: &'a str, body: &'a str },
    Status { repo: &'a Repo, actor: &'a User, sha: &'a str, status: &'a Status },
}

static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();

fn wake() -> &'static tokio::sync::Notify {
    WAKE.get_or_init(tokio::sync::Notify::new)
}

fn user_json(u: Option<&User>) -> Value {
    match u {
        Some(u) => json!({ "login": u.name, "id": u.id }),
        None => json!({ "login": "ghost", "id": 0 }),
    }
}

impl Bus<'_> {
    fn repo_json(&self, r: &Repo) -> Value {
        json!({
            "id": r.id,
            "name": r.name,
            "full_name": format!("{}/{}", r.owner, r.name),
            "owner": { "login": r.owner },
            "private": r.private,
            "html_url": format!("{}/{}/{}", self.url, r.owner, r.name),
            "clone_url": format!("{}/{}/{}.git", self.url, r.owner, r.name),
        })
    }

    fn issue_json(&self, r: &Repo, i: &Issue) -> Value {
        let kind = if i.is_pr { "pulls" } else { "issues" };
        json!({
            "number": i.number,
            "title": i.title,
            "body": i.body,
            "state": if i.closed { "closed" } else { "open" },
            "user": { "login": i.author.clone().unwrap_or_else(|| "ghost".into()) },
            "labels": i.labels.iter().map(|l| json!({ "name": l.name, "color": l.color })).collect::<Vec<_>>(),
            "html_url": format!("{}/{}/{}/{kind}/{}", self.url, r.owner, r.name, i.number),
        })
    }

    fn pull_json(&self, r: &Repo, i: &Issue, p: &Pull) -> Value {
        let mut v = self.issue_json(r, i);
        v["merged"] = json!(p.merged);
        v["merge_commit_sha"] = json!(p.merge_sha);
        v["head"] = json!({ "ref": p.head_branch, "sha": p.head_sha });
        v["base"] = json!({ "ref": p.base_branch });
        v
    }
}

/// Fan an event out. Never fails the caller: problems are logged.
pub fn emit(bus: &Bus<'_>, ev: Ev<'_>) {
    let db = bus.db;
    let (repo, event, payload, activity): (&Repo, &str, Value, Option<(Option<i64>, String, String)>) = match &ev {
        Ev::Push { repo, pusher, refname, before, after, commits } => {
            let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
            let deleted = after.bytes().all(|b| b == b'0');
            let payload = json!({
                "ref": refname, "before": before, "after": after, "deleted": deleted,
                "repository": bus.repo_json(repo), "pusher": user_json(*pusher), "sender": user_json(*pusher),
                "commits": commits.iter().take(20).map(|(sha, msg)| json!({
                    "id": sha, "message": msg, "url": format!("{}/{}/{}/commit/{sha}", bus.url, repo.owner, repo.name)
                })).collect::<Vec<_>>(),
            });
            let data = format!("{branch}\t{}\t{after}\t{}", commits.len(), if deleted { "deleted" } else { "" });
            (*repo, "push", payload, Some((pusher.map(|u| u.id), "push".to_owned(), data)))
        }
        Ev::Issue { repo, actor, issue, action } => {
            let payload = json!({ "action": action, "issue": bus.issue_json(repo, issue), "repository": bus.repo_json(repo), "sender": user_json(Some(actor)) });
            let data = format!("{}\t{}\t{action}", issue.number, issue.title);
            (*repo, "issues", payload, Some((Some(actor.id), "issue".to_owned(), data)))
        }
        Ev::Comment { repo, actor, issue, body } => {
            let payload = json!({
                "action": "created", "issue": bus.issue_json(repo, issue),
                "comment": { "body": body, "user": user_json(Some(actor)) },
                "repository": bus.repo_json(repo), "sender": user_json(Some(actor)),
            });
            let data = format!("{}\t{}\t{}", issue.number, issue.title, if issue.is_pr { "pr" } else { "" });
            (*repo, "issue_comment", payload, Some((Some(actor.id), "comment".to_owned(), data)))
        }
        Ev::Pull { repo, actor, issue, pull, action } => {
            // GitHub shape: a merge is "closed" with pull_request.merged = true.
            let wire_action = if *action == "merged" { "closed" } else { action };
            let payload = json!({
                "action": wire_action, "number": issue.number, "pull_request": bus.pull_json(repo, issue, pull),
                "repository": bus.repo_json(repo), "sender": user_json(*actor),
            });
            let data = format!("{}\t{}\t{action}", issue.number, issue.title);
            let act = (*action != "synchronize").then(|| (actor.map(|u| u.id), "pull".to_owned(), data));
            (*repo, "pull_request", payload, act)
        }
        Ev::Review { repo, actor, issue, pull, state, body } => {
            let payload = json!({
                "action": "submitted",
                "review": { "state": state, "body": body, "user": user_json(Some(actor)) },
                "pull_request": bus.pull_json(repo, issue, pull),
                "repository": bus.repo_json(repo), "sender": user_json(Some(actor)),
            });
            let data = format!("{}\t{}\t{state}", issue.number, issue.title);
            (*repo, "pull_request_review", payload, Some((Some(actor.id), "review".to_owned(), data)))
        }
        Ev::Status { repo, actor, sha, status } => {
            let payload = json!({
                "sha": sha, "state": status.state, "context": status.context,
                "description": status.description, "target_url": status.target_url,
                "repository": bus.repo_json(repo), "sender": user_json(Some(actor)),
            });
            (*repo, "status", payload, None)
        }
    };

    if let Some((actor, kind, data)) = activity {
        if let Err(e) = db.add_activity(repo.id, actor, &kind, &data) {
            eprintln!("activity: {e}");
        }
    }
    queue_hooks(db, repo, event, &payload);
    notify(db, &ev);
}

fn queue_hooks(db: &Db, repo: &Repo, event: &str, payload: &Value) {
    let Ok(hooks) = db.webhooks(repo.id) else { return };
    let body = payload.to_string();
    let mut queued = false;
    for h in hooks.iter().filter(|h| h.wants(event)) {
        match db.queue_delivery(h.id, event, &body) {
            Ok(_) => queued = true,
            Err(e) => eprintln!("queue webhook {}: {e}", h.id),
        }
    }
    if queued {
        wake().notify_one();
    }
}

/// Nudge the delivery worker (after re-queueing by hand).
pub fn queue_ping_wake() {
    wake().notify_one();
}

pub fn queue_ping(db: &Db, bus_url: &str, repo: &Repo, hook_id: i64) {
    let bus = Bus { db, url: bus_url };
    let body = json!({ "zen": "Keep it small.", "hook_id": hook_id, "repository": bus.repo_json(repo) }).to_string();
    if db.queue_delivery(hook_id, "ping", &body).is_ok() {
        wake().notify_one();
    }
}

/// Who hears about an issue/PR event: watchers, participants, mentioned
/// users — never the actor, and only people who can read the repo.
fn notify(db: &Db, ev: &Ev<'_>) {
    let (repo, actor, issue, text): (&Repo, Option<&User>, &Issue, &str) = match ev {
        Ev::Issue { repo, actor, issue, action } => (repo, Some(*actor), issue, if *action == "opened" { issue.body.as_str() } else { "" }),
        Ev::Comment { repo, actor, issue, body } => (repo, Some(*actor), issue, body),
        Ev::Pull { repo, actor, issue, action, .. } if *action != "synchronize" => {
            (repo, *actor, issue, if *action == "opened" { issue.body.as_str() } else { "" })
        }
        Ev::Review { repo, actor, issue, body, .. } => (repo, Some(*actor), issue, body),
        _ => return,
    };
    let actor_id = actor.map(|u| u.id);
    let mut recipients: Vec<(User, &'static str)> = Vec::new();
    let mut add = |u: User, reason: &'static str| {
        if Some(u.id) == actor_id {
            return;
        }
        match recipients.iter_mut().find(|(x, _)| x.id == u.id) {
            Some(e) => {
                // Keep the most specific reason.
                let rank = |r: &str| ["mention", "assigned", "author", "participating", "watching"].iter().position(|x| *x == r).unwrap_or(9);
                if rank(reason) < rank(e.1) {
                    e.1 = reason;
                }
            }
            None => recipients.push((u, reason)),
        }
    };
    for seg in crate::refs::scan(text) {
        if let crate::refs::Segment::Ref(crate::refs::Token::User(name), _) = seg {
            if let Ok(Some(u)) = db.user_by_name(&name) {
                add(u, "mention");
            }
        }
    }
    for (u, reason) in db.participants(issue.id).unwrap_or_default() {
        add(u, reason);
    }
    for u in db.watchers(repo.id).unwrap_or_default() {
        add(u, "watching");
    }
    for (u, reason) in recipients {
        if db.access(Some(&u), repo).is_ok_and(|a| a >= Access::Read) {
            let _ = db.notify(u.id, repo.id, issue.id, reason, actor_id);
        }
    }
}

// ---------- delivery (server only) ----------

const MAX_ATTEMPTS: i64 = 5;
const BACKOFF: [i64; 4] = [10, 60, 300, 1800];

fn sign(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    let out = mac.finalize().into_bytes();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Deliver everything that's due. Blocking; run on the blocking pool.
fn deliver_due(db: &Db) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent("loom-hookshot")
        .build()
        .into();
    for _ in 0..10 {
        let Ok(batch) = db.due_deliveries(20) else { return };
        if batch.is_empty() {
            return;
        }
        for (d, url, secret) in batch {
            let start = std::time::Instant::now();
            let mut req = agent
                .post(&url)
                .header("Content-Type", "application/json")
                .header("X-Loom-Event", &d.event)
                .header("X-GitHub-Event", &d.event)
                .header("X-Loom-Delivery", d.id.to_string());
            if !secret.is_empty() {
                let sig = format!("sha256={}", sign(&secret, d.payload.as_bytes()));
                req = req.header("X-Loom-Signature-256", &sig).header("X-Hub-Signature-256", &sig);
            }
            let (status, response) = match req.send(&d.payload) {
                Ok(mut r) => {
                    let code = r.status().as_u16() as i64;
                    let text = r.body_mut().with_config().limit(4096).read_to_string().unwrap_or_default();
                    (Some(code), text)
                }
                Err(e) => (None, e.to_string()),
            };
            let ms = start.elapsed().as_millis() as i64;
            let ok = status.is_some_and(|s| (200..300).contains(&s));
            let attempts = d.attempts + 1;
            let done = ok || attempts >= MAX_ATTEMPTS;
            let next = crate::db::now() + BACKOFF[(attempts as usize - 1).min(BACKOFF.len() - 1)];
            let response: String = response.chars().take(2000).collect();
            if let Err(e) = db.record_attempt(d.id, status, &response, ms, done, next) {
                eprintln!("record delivery {}: {e}", d.id);
            }
        }
    }
}

/// Server task: deliver on wake-ups (in-process events) and every few
/// seconds (events queued by hook processes, retries).
pub async fn run_deliveries(app: std::sync::Arc<crate::app::App>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = wake().notified() => {}
        }
        // Only wake a blocking thread when there's work: idle stays 1 thread.
        if !app.db.has_due_deliveries() {
            continue;
        }
        let a = app.clone();
        let _ = tokio::task::spawn_blocking(move || deliver_due(&a.db)).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 2.
        assert_eq!(
            super::sign("Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
