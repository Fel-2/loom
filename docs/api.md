# API, webhooks and LFS

Everything here is stable as of 0.1.0 unless a line says otherwise. There is no
version-negotiation header: the base path is `/api/v1`.

## Authentication

| How | Header | Use |
|---|---|---|
| Token | `Authorization: Bearer loom_…` | scripts, CI |
| Token as password | `Authorization: Basic base64(user:loom_…)` | tools that only do Basic |
| Nothing | — | reads of **public** repos |

- Tokens are created in the web UI at `/settings/tokens` and shown once.
- Cookies are ignored by the API on purpose, so a browser cannot be tricked into
  calling it; there is no CSRF surface here.
- A token that is present but wrong is a `401`, never a silent downgrade to
  anonymous.
- Permissions are the same as the web UI: read, write or admin per repository,
  plus org membership.

Errors are JSON, never HTML:

```json
{ "message": "Write access required" }
```

`404` is also returned instead of `403` when the caller may not see the
repository at all, so private names do not leak.

## Endpoints

Base: `http://your-host:3000/api/v1`

| Method | Path | Notes |
|---|---|---|
| GET | `/user` | the token's user; `401` when anonymous |
| GET | `/repos/{owner}/{name}` | repo metadata |
| GET | `/repos/{owner}/{name}/branches` | `[{ "name", "commit": { "sha" } }]` |
| GET | `/repos/{owner}/{name}/issues` | `?state=open\|closed&label=&page=` |
| POST | `/repos/{owner}/{name}/issues` | `{"title","body"}`, 201 |
| GET | `/repos/{owner}/{name}/issues/{number}` | |
| PATCH | `/repos/{owner}/{name}/issues/{number}` | any of `title`, `body`, `state` (`open`/`closed`), `labels` (array of names), `milestone` (title, `""` clears); author or write access |
| GET | `/repos/{owner}/{name}/issues/{number}/comments` | |
| POST | `/repos/{owner}/{name}/issues/{number}/comments` | `{"body"}`, 201 |
| GET | `/repos/{owner}/{name}/pulls` | same filters as issues |
| POST | `/repos/{owner}/{name}/pulls` | `{"base","head","title","body","draft"}`, 201; `409` if an open PR already proposes those changes |
| GET | `/repos/{owner}/{name}/pulls/{number}` | includes `head`, `base`, `merge_commit_sha`, `draft` |
| POST | `/repos/{owner}/{name}/pulls/{number}/merge` | `{"strategy":"merge"\|"squash"\|"rebase","expected_head"}`, both optional; required checks and drafts still block |
| GET | `/repos/{owner}/{name}/labels` | |
| POST | `/repos/{owner}/{name}/labels` | `{"name","color":"#rrggbb","description"}`, 201; `409` on a duplicate name |
| PATCH | `/repos/{owner}/{name}/labels/{id}` | same body as create |
| DELETE | `/repos/{owner}/{name}/labels/{id}` | 204 |
| GET | `/repos/{owner}/{name}/milestones` | |
| POST | `/repos/{owner}/{name}/milestones` | `{"title","description","due"}`, 201 |
| PATCH | `/repos/{owner}/{name}/milestones/{id}` | same body, plus `closed` |
| DELETE | `/repos/{owner}/{name}/milestones/{id}` | 204 |
| GET | `/repos/{owner}/{name}/releases` | drafts only with write access |
| POST | `/repos/{owner}/{name}/releases` | `{"tag_name","name","body","draft","prerelease"}`, 201; the tag must exist |
| PATCH | `/repos/{owner}/{name}/releases/{id}` | same body; the tag must not change |
| DELETE | `/repos/{owner}/{name}/releases/{id}` | 204 |
| GET | `/repos/{owner}/{name}/hooks` | write access |
| POST | `/repos/{owner}/{name}/hooks` | `{"url","secret","events":["push",…],"active"}`, 201; a ping is queued unless `active:false` |
| DELETE | `/repos/{owner}/{name}/hooks/{id}` | 204 |
| GET | `/repos/{owner}/{name}/statuses/{sha}` | all statuses for a commit |
| POST | `/repos/{owner}/{name}/statuses/{sha}` | report a status (write access) |
| GET | `/repos/{owner}/{name}/commits/{rev}/status` | combined state |
| GET | `/healthz`, `/api/healthz` | liveness, `503` when the DB or storage is broken |

`{sha}` and `{rev}` accept a full SHA, a short SHA, a branch or a tag.

Lists return an array, 50 per page, `?page=N`. There is no total-count field —
`?state=` plus a short page means "you are done".

### Create an issue

```sh
curl -X POST http://localhost:3000/api/v1/repos/fel/loom/issues \
  -H "Authorization: Bearer $LOOM_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"title": "Tree view scrolls sideways on 80 columns", "body": "Steps:\n1. ..."}'
```

```json
{
  "number": 7, "title": "...", "body": "...", "state": "open",
  "user": { "login": "fel" }, "comments": 0, "labels": [], "milestone": null,
  "created_at": 1763470000, "updated_at": 1763470000, "closed_at": null,
  "pull_request": false, "merged": null,
  "html_url": "http://localhost:3000/fel/loom/issues/7"
}
```

Timestamps are Unix seconds.

### Commit statuses

CI reports one status per `context`; the newest wins.

```sh
curl -X POST http://localhost:3000/api/v1/repos/fel/loom/statuses/$SHA \
  -H "Authorization: Bearer $LOOM_TOKEN" -H 'Content-Type: application/json' \
  -d '{"state":"success","context":"ci/build","description":"2m14s","target_url":"https://ci.example/42"}'
```

- `state`: `pending`, `success`, `failure` or `error`
- `context` defaults to `default`, max 100 chars
- `target_url` must be `http(s)`, max 2000 chars

`GET .../commits/{rev}/status` combines them:

```json
{ "state": "pending", "sha": "…", "total_count": 2, "statuses": [ … ] }
```

Combination rule: any `failure`/`error` wins, then any `pending` (and an empty
list counts as `pending`), otherwise `success`. Branch protection can require
the combined state to be `success` before a push to a protected branch.

## Webhooks

Add one in *repository → settings → hooks*: a URL, an optional secret, and which
events to send. Adding a hook fires `ping` immediately, so you can see whether
the endpoint is reachable.

| Event | Fired when |
|---|---|
| `ping` | the hook is created |
| `push` | a push to any branch (includes up to 20 commits) |
| `issues` | opened, closed, reopened, edited |
| `issue_comment` | a comment is created |
| `pull_request` | opened, closed, reopened, synchronize (`merged` arrives as `closed` with `merged: true`) |
| `pull_request_review` | submitted |
| `status` | a commit status is reported |

Deliveries carry GitHub-compatible headers, so GitHub-receiving code usually
works unchanged:

```
X-GitHub-Event: push
X-Loom-Event: push
X-Loom-Delivery: 1234
X-Hub-Signature-256: sha256=…
X-Loom-Signature-256: sha256=…
```

The signature is `HMAC-SHA256(secret, raw_body)`, present only when a secret is
set. Verify it against the **raw** body, before any JSON parsing.

Retries: 5 attempts total, backing off 10 s, 60 s, 5 min, 30 min. Every attempt
is recorded in the repository's hook page with the status code, response snippet
and timestamp. There is no automatic disabling of a failing hook.

Payloads use the `{"action", "repository", "sender", …}` shape. `repository`
matches the API's repo object. Releases do **not** emit a webhook yet.

## Git LFS

Both the standard and the short paths work:

```
POST /{owner}/{name}/info/lfs/objects/batch
GET  /{owner}/{name}/info/lfs/objects/{oid}
POST /{owner}/{name}/info/lfs/objects/{oid}/verify
```

Uploads are verified against the advertised SHA-256. Authentication is the same
as git: a password or a token, Basic or Bearer. Over SSH, `git-lfs-authenticate`
hands `git-lfs` a short-lived signed token, so `git lfs push` works on SSH
remotes without any extra configuration.

The largest single object is `LOOM_LFS_MAX_MB` (default 2048). Objects unreferenced
by any ref are deleted by `loom lfs gc --repo owner/name`.

## Rate limits

Per IP, in memory, token bucket with a two-second burst: `LOOM_RATE_READ`
(default 2000/s) and `LOOM_RATE_WRITE` (default 100/s). Over the limit you get
`429` with `Retry-After: 1`. `/healthz`, `/metrics`, `/static/` and `/avatar/`
are never limited. Behind a reverse proxy set `LOOM_TRUST_PROXY=1` or every
client shares one bucket.

## What is not here

No GraphQL, no organisation or user management endpoints, no release or asset
endpoints, no webhook redelivery endpoint, and no search endpoint — search is
web only. Opening an issue and reporting a status are the two write paths that
exist today.
