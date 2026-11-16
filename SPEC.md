# loom — spec

A self-hosted git platform. Goal: the lowest resource use of any full-featured
forge, with a UI that breaks from the GitHub/GitLab/Gitea layout (inspired by
Tangled, whose code is MIT; no Tangled brand assets are used).

## Hard constraints

- One static binary. No Redis, no job queue, no external DB, no Node at runtime.
- Idle target: < 20 MB RSS, ~0% CPU. Benchmarked against Forgejo on the same repos.
- Forgejo baseline (2026-11-16, user's instance, idle): `gitea web` 125 MB / 43
  threads, postgres connections 61 MB (main server + background workers not
  counted), forgejo-runner 19 MB / 15 threads. loom must stay well under the
  125 MB server figure at feature parity. Re-measure after every milestone.
- loom M1 measured (browse only): 4–6 MB idle, ~61 MB peak at 32 concurrent,
  ~11 MB after load, 1 thread at idle.
- loom M2 measured (+ accounts, SQLite, embedded SSH, smart HTTP): 8.6 MB idle,
  ~52 MB peak at 32 concurrent browse requests, ~17 MB after load, 1 thread at
  idle. 16 concurrent clones (8 SSH + 8 HTTP) add nothing to loom's own RSS
  (pack work happens in short-lived `git` children).
- loom M3 measured (+ orgs, forks, stars, collaborators, branch protection
  via pre-receive hook): 8.8 MB idle, ~54 MB peak at 32 concurrent, 12.1 MB
  after load once the periodic `malloc_trim` runs (1.7 MB of it heap; the rest
  is the mapped binary and pack files). Without trim it held at 27 MB (not a
  leak: flat across repeated load rounds).
- loom M4 measured (+ issues, comments, labels, milestones, assignees,
  references, post-receive commit linking): 9.3 MB idle, ~52 MB peak at 32
  concurrent (mix incl. issue pages), 14.9 MB after load + trim. Issue list
  and issue page render in < 1 ms.
- loom M5 measured (+ pull requests): 9.6 MB idle, ~44 MB peak at 32
  concurrent (mix incl. PR conversation/files/commits/revisions), 16.7 MB
  after load + trim. PR files tab with a 430 KB diff: 6 ms; conversation:
  12 ms (several git subprocesses for mergeability: a cache candidate).
- loom M6 measured (+ webhooks, statuses, API, notifications, search):
  10 MB idle / 1 thread (delivery worker only spawns a thread when work is
  due), ~50 MB peak at 32 concurrent, 20 MB RSS after load + trim — of which
  11 MB is the mapped 13.3 MB binary (file-backed, reclaimable) and 6.5 MB
  private (heap 5.7 MB). Issue search 2 ms, API 2 ms, code search 19 ms.
- No federation.
- Git reads in-process via gitoxide (`gix`). Spawn `git` only for
  `upload-pack` / `receive-pack` (clone/fetch/push) and merges.
- SQLite (WAL) is the only database. FTS5 for search.
- Server-rendered HTML (maud, compiled in). CSS/JS embedded. Pages work without JS.
- Anything keyed by an immutable git SHA is cached forever and served with
  `ETag = sha`.
- No background workers. Webhook delivery is fire-and-forget on the runtime,
  with retry recorded in SQLite.

## Data layout

```
$LOOM_DATA/
  loom.db              # users, orgs, keys, issues, PRs, hooks, sessions
  repos/<owner>/<name>.git
  cache/               # rendered blobs/diffs keyed by sha (optional, bounded)
```

Backup = copy `loom.db` (via `.backup`) + `repos/`.

## Features

### Accounts
- Local users, argon2id passwords, cookie sessions
- SSH public keys per user
- Personal access tokens (HTTP push, API)
- Orgs (optional): members, teams with read/write/admin
- Per-repo collaborators with read/write/admin
- Admin via CLI first: `loom admin user add|del|passwd`, `loom admin repo ...`

### Transport
- Embedded SSH server (`russh`) — keys from DB, no OpenSSH config
- Smart HTTP clone/fetch (anonymous for public repos) and push (token/basic)
- Pull mirrors (periodic fetch, timer-driven, no worker pool)
- Git LFS — later

### Repos
- Create, fork, rename, transfer, archive, delete
- Public / private visibility
- Default branch, branch protection (no force-push, require PR)
- Topics, description, website
- Stars, watch

### Browsing
- Tree, blob, raw, blame
- Log (per path), commit view, diff, compare `a...b`
- Branches, tags, releases with attached assets
- README rendering (markdown, `pulldown-cmark`)
- Syntax highlighting, lazily computed and cached by blob sha

### Issues
- Open/close/reopen, comments (markdown), labels, milestones, assignees
- Cross references (`#12`, `owner/repo#12`, commit sha mentions)
- "Fixes #N" closes on merge to default branch

### Pull requests (branch-based)
- Same-repo or from fork
- Diff, commits tab, inline review comments
- Reviews: comment / approve / request changes
- Mergeability check, merge strategies: merge commit, squash, rebase (ff)
- Each force-push is kept as a reviewable revision (range-diff between rounds)

### Integrations
- Webhooks: push, tag, issue, PR, review, release; HMAC-SHA256 signed
- Commit status API so external CI (Jenkins etc.) can report back
- REST JSON API with tokens

### Misc
- Notifications (in-app), activity feed
- Search: repos, issues, PRs (FTS5); code search via `git grep` on demand

## Milestones

1. ✅ **Browse** — serve a directory of bare repos: tree/blob/log/commit/diff/refs + README. The new UI.
2. ✅ **Accounts + transport** (tests/smoke.sh) — users, sessions, SSH keys, embedded SSH, smart HTTP.
3. **Repo management** — create/fork/settings/collaborators/orgs/visibility. ✅ (tests/m3.sh)
4. ✅ **Issues** (tests/m4.sh)
5. ✅ **Pull requests** (tests/m5.sh)
6. ✅ **Webhooks, statuses, API, search, notifications** (tests/m6.sh)
7. ✅ **Benchmark harness vs Forgejo** (bench/run.sh, bench/results/SUMMARY.md) — run early, run often.
   First run (2026-11-16, Forgejo 15.0.6 + SQLite, same data): idle 16.6 vs
   239 MB, after load 16.9 vs 305 MB, peak under 32 clients 87.5 vs 354 MB
   (988 MB with Forgejo's git children), 1 vs 43–104 threads, 564 vs 49 req/s.
   After fixes (handle cache, PR summary cache, git log for path history):
   1,176 req/s at the same 4 threads; private memory under load 11 MB.

## 8. Features pass (after the benchmark)

Highlighting (dependency-free scanner, 128 KB cap), blob view capped at 5000
lines and diffs at 4000 lines/200 files, blame (git blame + bounded memo),
releases with uploaded assets and `git archive` tarballs, TOTP two-factor with
`loom user 2fa-off` recovery, pull mirrors (read-only repos, timer-driven),
per-(IP,user) login throttling, and a cap of 8 concurrent streaming git
processes (`LOOM_MAX_GIT`). tests/m7.sh: 48 checks.

Still out: Git LFS, per-file last-commit badges (cost ~1 s per large directory,
which is exactly the latency loom avoids).

## 9. LFS and operations

Git LFS (batch API, SHA-256-verified uploads, authenticated downloads,
`git-lfs-authenticate` over SSH with short-lived signed tokens; storage under
`data/lfs/`). Deployment: two-stage Dockerfile (runtime carries git), compose
file, hardened systemd unit; `/healthz` and `/metrics`; `loom backup <dir>
[--repos]` (SQLite VACUUM INTO + reflink copy) and `loom version`; startup
fails fast if git is missing. tests/m8.sh: 21 checks.

## 10. Completeness pass

Issue/PR templates, draft and pre-release releases, identicon avatars, commit
graph view, per-repo opt-in last-commit badges, org teams with per-repo grants,
email notifications via `LOOM_MAIL_CMD` (no SMTP code in loom), `loom lfs gc`
(with orphan-repo cleanup; SIGPIPE-safe CLI output), per-IP rate limiting.
Fixed along the way: repo row ids are reused after deletion, so delete now
drops the repo's LFS objects and release assets; GC enumerates objects without
deadlocking on pipe buffers. tests/m9.sh: 42 checks.
