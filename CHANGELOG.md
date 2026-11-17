# Changelog

## 0.1.0 — 2026-11-17

First public release. One static binary, SQLite, git; no Redis, no job queue, no
external database.

- **Browse**: tree, blob with a built-in highlighter, raw, log, path-limited
  history, commit, diff, branches and tags, blame, README rendering, commit
  graph. ETag/304 on everything keyed by a git id; gzip on text responses.
- **Accounts**: local users, argon2id, sessions, SSH keys, API tokens, optional
  TOTP 2FA, orgs, org teams, per-repo collaborators.
- **Transport**: embedded SSH server (no OpenSSH needed) and smart HTTP, both
  streaming into short-lived `git upload-pack`/`receive-pack` processes. Pull
  mirrors for read-only upstream tracking.
- **Repos**: settings, branch protection, archive, rename, transfer, delete to a
  recoverable trash, hardlinked forks, stars, watch.
- **Issues and PRs**: shared numbering, labels, milestones, assignees, timeline,
  cross-references, `Fixes #12` auto-close; fork heads, revisions with
  `git range-diff`, reviews, no-JS inline comments, merge/squash/rebase.
- **Git LFS**: batch API, SHA-256 verified uploads, `git-lfs-authenticate` over
  SSH, `loom lfs gc`.
- **Releases**: notes per tag, drafts, pre-releases, uploaded assets, source
  tarballs.
- **Integrations**: signed webhooks with retries and a delivery log, commit
  statuses API, REST API v1 with bearer tokens, in-app notifications, activity
  feed, FTS5 search and `git grep` code search.
- **Operations**: `loom doctor`, `loom backup --repos`, `loom restore`,
  `loom repo maintain`, `/healthz`, `/metrics`, systemd and runit units,
  container image.
- **Measured** against Forgejo 15.0.6 with SQLite on the same data: 16.6 MB idle
  vs 239 MB, 1 thread vs 43, 1,176 vs 49 requests/s under 32 clients. See
  `bench/results/SUMMARY.md`.

Known limitations: no federation, no built-in CI runner, no package registry,
no wiki, no SSO/OIDC/LDAP, no audit log. Per-file "last commit" badges are opt-in
per repository because they cost about a second per large directory.
