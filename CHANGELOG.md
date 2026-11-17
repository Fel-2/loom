# Changelog

## 0.2.0 — 2026-11-17

Migration and hardening pass after 0.1.0. Same database: the schema
migrates in place on first start (v8).

- **Migration**: `scripts/import-forgejo.sh --issues --releases` now copies
  labels, milestones, issues (with comments, state and PRs excluded),
  and release notes from a Forgejo/Gitea instance through the loom API; the
  script mints its own one-time token via the new `loom token add` CLI.
  Numbering is per-instance, so issues are renumbered and the mapping is
  printed. Tested in `tests/m10.sh` against a stub Gitea API.
- **API v1 grown**: `PATCH` an issue (title/body/state/labels/milestone),
  list comments, labels CRUD, milestones CRUD, releases CRUD, PR create
  (drafts included) + merge, webhooks CRUD. Enough surface for a full
  migration and for automation scripts.
- **Draft pull requests**: mark a PR as a draft when opening it (web form or
  API), merge is refused until it is marked ready; the author can flip back.
- **Atom feeds**: `/atom.xml`, `/issues.atom.xml`, `/releases.atom.xml` per
  repository, ETagged; private repos authenticate with a session cookie or
  an API token (bad token = 401, never a silent downgrade).
- **Reactions**: 👍 👎 😄 😕 ❤️ 🎉 🚀 👀 on issues and PRs, no-JS toggles.
- **Security**: `Content-Security-Policy` on every response — the UI ships no
  scripts, so everything is denied except the stylesheet, same-origin
  images and forms (`SECURITY.md` gap closed).
- **Portability**: `scripts/dist.sh` builds an aarch64 static tarball
  (`ARCH=aarch64`), and CI builds both tarballs; the runtime container
  image was already multi-arch.
- **Operations**: `bench/guard.sh` fails the build if idle RSS after a load
  burst exceeds its budget (runs in CI; measured 13 MB against a 32 MB
  budget on the laptop).
- **Tests**: unit tests for the access matrix (owner/collaborator/org/team/
  admin over public and private repos); suite counts now
  smoke 25 · m3 63 · m4 66 · m5 81 · m6 124 · m7 48 · m8 21 · m9 43 · m10 38.

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
