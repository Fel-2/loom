# loom

A self-hosted git forge in one small Rust binary. No federation, no Redis, no
job queue, no separate database server: SQLite and git, nothing else.

Measured against Forgejo 15.0.6 with SQLite on the same data (83k-commit
`git/git`, issues, a PR): **16.6 MB idle / 1 thread vs 239 MB / 43 threads**,
~1,180 vs 49 requests/s under 32 clients, 15 MB RSS after load.
Full numbers and method: [`bench/results/SUMMARY.md`](bench/results/SUMMARY.md).

## What it does

- **Browse**: tree, blob (with a small built-in syntax highlighter), raw, log,
  path-limited history, commit + diff, branches & tags, blame, README rendering.
  Everything revalidates by ETag; raw files are cacheable forever by content.
- **Accounts**: local users, argon2id passwords, sessions, SSH keys, API tokens,
  optional TOTP two-factor (with `loom user 2fa-off` as recovery).
- **Orgs**: members with read/write/admin across the org's repositories.
- **Transport**: embedded SSH server (no OpenSSH needed) and smart HTTP, both
  streaming into short-lived `git upload-pack`/`receive-pack` processes.
- **Repos**: settings, collaborators, branch protection (no force-push, no
  delete, PRs only, required status checks), archive, rename, transfer, delete
  (moved to `data/trash/`, recoverable), forks (hardlinked), stars, watch.
- **Pull mirrors**: periodic fetch from an upstream URL; mirrored repos are
  read-only.
- **Issues & PRs**: shared numbering, labels, milestones, assignees, timeline;
  `#12` / `owner/repo#12` / `@user` / sha references that only link when the
  viewer may see the target; `Fixes #12` closes on merge or on push to the
  default branch. PRs: fork heads, revisions per push with `git range-diff`,
  reviews, no-JS inline comments, merge/squash/rebase, "merged by push".
- **Git LFS**: batch API, uploads verified by SHA-256, `git-lfs-authenticate`
  over SSH so LFS works with SSH remotes too; storage under `data/lfs/`.
- **Releases**: notes per tag, uploaded assets with download counts, source
  tarballs (`git archive`).
- **Integrations**: signed webhooks with retries and a delivery log, commit
  statuses API + combined status, REST API v1 with bearer tokens,
  notifications, activity feed, full-text search (issues/PRs/comments) and
  code search (`git grep`).

## Run it

```sh
cargo build --release
LOOM_DATA=./data LOOM_ADDR=127.0.0.1:3000 LOOM_URL=http://127.0.0.1:3000 \
  ./target/release/loom serve

# create the first admin (reads the password from stdin)
printf 'a-long-password' | ./target/release/loom user add you --admin --password-stdin
./target/release/loom repo create you/hello --desc "first repo"
```

`loom --help` lists the admin commands (`user`, `key`, `repo`, `org`, `hook`).

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `LOOM_DATA` | `./data` | Database, repositories, hooks, trash, release assets |
| `LOOM_ADDR` | `127.0.0.1:3000` | HTTP listen address |
| `LOOM_URL` | `http://$LOOM_ADDR` | Public URL (clone links, webhook payloads) |
| `LOOM_SSH_ADDR` | `127.0.0.1:2222` | SSH listen address; `off` disables |
| `LOOM_SSH_HOST` | same as SSH addr | Host:port shown in SSH clone URLs |
| `LOOM_TRUST_PROXY` | unset | `1` trusts `X-Forwarded-For` (behind a reverse proxy) |
| `LOOM_GIT_THREADS` | `4` | Blocking threads for git work |
| `LOOM_MAX_GIT` | `8` | Max concurrent streaming git processes (clones/pushes) |

Backups: `sqlite3 data/loom.db ".backup out.db"` plus `data/repos/`.

## Deploy

- **Container**: `docker compose up -d` (Dockerfile is two-stage; the runtime
  image carries git). First admin:
  `docker compose exec loom loom user add you --admin --password-stdin`.
  The container runs as uid 10001, so a bind-mounted `./data` is owned by that
  uid — either `chown -R 10001 data` on the host (if you want direct access for
  backups) or use a named volume.
- **systemd**: `deploy/loom.service` (hardened: loom can only write
  `LOOM_DATA`). Put a reverse proxy in front for TLS and set `LOOM_URL` to the
  public URL; add `LOOM_TRUST_PROXY=1` if the proxy sets `X-Forwarded-For`.
- **Health / metrics**: `GET /healthz` (JSON, 503 when the DB or storage is
  broken) and `GET /metrics` (Prometheus text: uptime, request counters,
  in-flight requests, free git slots, RSS, threads, repo count).
- **Backups**: `loom backup /backups --repos` writes a consistent SQLite
  snapshot (`VACUUM INTO`) plus a copy of the repositories (reflink when the
  filesystem supports it). Run it from cron; restore is "stop loom, put the
  files back".
- **Upgrades**: replace the binary and restart. Git hooks call the binary by
  path, resolved at startup, so an in-place upgrade doesn't break pushes
  (`loom repo maintain` rebuilds commit-graphs if you changed git versions).

## Tests

`tests/*.sh` run against a live instance (default `127.0.0.1:3917`, seeded with
users `alice` (admin, `alice-test-pw`), `bob` (`bob-test-pw1`), repos
`alice/ytm-tui`, `alice/secret`, `alice/slate`, `git/git`, and test keys in
`~/.cache/loom-bench/keys`). They always run git with an empty config, so no
credential helper or GUI prompt is involved.

```sh
tests/smoke.sh   # transport + permissions (25)
tests/m3.sh      # repo management, orgs, protection, forks (63)
tests/m4.sh      # issues, labels, milestones, references (65)
tests/m5.sh      # pull requests, merges, revisions (71)
tests/m6.sh      # webhooks, statuses, API, notifications, search (61)
tests/m7.sh      # highlighting, caps, blame, releases, 2FA, mirrors (48)
tests/m8.sh      # LFS (real git-lfs), healthz, metrics, backup, version (21)
cargo test --release   # unit tests (TOTP, HMAC, tokenizer, highlighter, ...)
```

`bench/` holds the Forgejo comparison harness.

## Not implemented

Per-file "last commit" badges in tree views — Forgejo spends ~1 s per large
directory on those, which is exactly the latency loom avoids. (Git LFS is
supported.)

## License

MIT.
