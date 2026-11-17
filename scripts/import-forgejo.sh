#!/usr/bin/env bash
# Import a Forgejo (or Gitea) user/org into loom: repositories via mirror
# clone + push, and — with --issues / --releases — issues with their labels,
# milestones and comments, and releases with their notes.
#
# Run this on the machine that runs loom: repositories are registered with the
# CLI (needs LOOM_DATA), and the script mints a one-time API token with
# `loom token add` for the metadata import. Needs python3 and curl.
#
#   # public repositories, no credentials needed on the Forgejo side
#   ./scripts/import-forgejo.sh --from http://localhost:3030 --owner Fel-2 \
#       --loom-owner fel
#
#   # private ones too, plus issues and releases
#   ./scripts/import-forgejo.sh --from https://git.example.com --owner me \
#       --token <forgejo-api-token> --loom-owner fel \
#       --issues --releases
#
# Not imported: pull requests (their branches are not in a mirror clone),
# release assets, stars, topics, wikis. Issues are renumbered — the old
# number is printed next to the new one, and `#old` references inside bodies
# keep pointing at the old numbers.
set -uo pipefail

FROM=""
OWNER=""
TOKEN=""
LOOM_OWNER=""
LOOM_URL="${LOOM_URL:-http://127.0.0.1:3000}"
LOOM_DATA="${LOOM_DATA:-$HOME/.local/share/loom}"
LOOM_BIN="${LOOM_BIN:-$(command -v loom || echo "$HOME/.local/bin/loom")}"
PUSH_BASE="${PUSH_BASE:-ssh://git@127.0.0.1:2223}"
DRY=0
FORCE=0
DO_ISSUES=0
DO_RELEASES=0

die() { echo "error: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

usage() {
    sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'USAGE'

Options:
  --from URL         Forgejo base URL (http://host:port)
  --owner NAME       Forgejo user or org whose repositories to import
  --token TOKEN      Forgejo API token (needed for private repositories)
  --loom-owner NAME  loom user that will own them (default: --owner, lowercased)
  --loom-url URL     loom's address for the API (default: http://127.0.0.1:3000)
  --loom-data DIR    loom data directory (default: $LOOM_DATA)
  --loom-bin PATH    loom binary (default: loom on PATH)
  --push-base URL    where to push (default: ssh://git@127.0.0.1:2223)
  --issues           also import labels, milestones, issues and comments
  --releases         also import releases (notes; not their asset files)
  --dry-run          list what would be imported and stop
  --force            import again even if the repository already exists
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --from) FROM=${2:-}; shift 2 ;;
        --owner) OWNER=${2:-}; shift 2 ;;
        --token) TOKEN=${2:-}; shift 2 ;;
        --loom-owner) LOOM_OWNER=${2:-}; shift 2 ;;
        --loom-url) LOOM_URL=${2:-}; shift 2 ;;
        --loom-data) LOOM_DATA=${2:-}; shift 2 ;;
        --loom-bin) LOOM_BIN=${2:-}; shift 2 ;;
        --push-base) PUSH_BASE=${2:-}; shift 2 ;;
        --issues) DO_ISSUES=1; shift ;;
        --releases) DO_RELEASES=1; shift ;;
        --dry-run) DRY=1; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

[ -n "$FROM" ] || die "--from is required"
[ -n "$OWNER" ] || die "--owner is required"
FROM=${FROM%/}
LOOM_URL=${LOOM_URL%/}
LOOM_OWNER=${LOOM_OWNER:-$(printf '%s' "$OWNER" | tr '[:upper:]' '[:lower:]')}
[ -x "$LOOM_BIN" ] || command -v "$LOOM_BIN" >/dev/null || die "loom binary not found: $LOOM_BIN"
[ -d "$LOOM_DATA" ] || die "no loom data directory: $LOOM_DATA"

export LOOM_DATA

api() {
    if [ -n "$TOKEN" ]; then
        curl -sfS -H "Authorization: token $TOKEN" "$@"
    else
        curl -sfS "$@"
    fi
}

say "reading repositories from $FROM (owner $OWNER)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Try the user endpoint, then the org endpoint: Gitea/Forgejo expose both.
list=""
for kind in users orgs; do
    if list=$(api "$FROM/api/v1/$kind/$OWNER/repos?limit=50&page=1"); then
        break
    fi
done
[ -n "$list" ] || die "could not list repositories for '$OWNER' (private repos need --token)"

page=1
: > "$tmp/repos.tsv"
while :; do
    if [ "$page" -eq 1 ]; then body=$list; else
        body=$(api "$FROM/api/v1/users/$OWNER/repos?limit=50&page=$page" 2>/dev/null) || break
        [ -n "$body" ] || break
    fi
    python3 - "$body" >> "$tmp/repos.tsv" <<'PY'
import json, sys
for r in json.loads(sys.argv[1]):
    print("\t".join([
        r["name"].lower(),
        r.get("default_branch") or "",
        "1" if r.get("private") else "0",
        (r.get("description") or "").replace("\t", " ").replace("\n", " "),
        r.get("clone_url") or "",
    ]))
PY
    count=$(printf '%s' "$body" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
    [ "$count" -ge 50 ] || break
    page=$((page + 1))
done

total=$(wc -l < "$tmp/repos.tsv")
[ "$total" -gt 0 ] || die "no repositories found for '$OWNER'"
say "$total repositories found"

existing=$("$LOOM_BIN" repo list 2>/dev/null | tr '[:upper:]' '[:lower:]')
imported=0; skipped=0; failed=0
: > "$tmp/done.tsv"

while IFS=$'\t' read -r name branch private desc clone_url; do
    [ -n "$name" ] || continue
    target="$LOOM_OWNER/$name"
    if printf '%s\n' "$existing" | grep -qxF "$(printf '%s' "$target" | tr '[:upper:]' '[:lower:]')" && [ "$FORCE" -eq 0 ]; then
        say "  skip  $target (already exists)"
        skipped=$((skipped + 1))
        continue
    fi
    if [ "$DRY" -eq 1 ]; then
        say "  would import $target (branch ${branch:-?}, private=$private)"
        continue
    fi

    args=(repo create "$target")
    [ -n "$desc" ] && args+=(--desc "$desc")
    [ "$private" = "1" ] && args+=(--private)
    if ! out=$("$LOOM_BIN" "${args[@]}" 2>&1); then
        say "  FAIL  $target: $(printf '%s' "$out" | tail -1)"
        failed=$((failed + 1))
        continue
    fi

    clone_url=${clone_url:-$FROM/$OWNER/$name.git}
    if ! git clone -q --mirror "$clone_url" "$tmp/$name.git" 2>"$tmp/clone.err"; then
        say "  FAIL  $target: clone: $(tail -1 "$tmp/clone.err")"
        failed=$((failed + 1))
        continue
    fi
    if ! git -C "$tmp/$name.git" push -q "$PUSH_BASE/$LOOM_OWNER/$name.git" \
            'refs/heads/*:refs/heads/*' 'refs/tags/*:refs/tags/*' 2>"$tmp/push.err"; then
        say "  FAIL  $target: push: $(tail -1 "$tmp/push.err")"
        failed=$((failed + 1))
        continue
    fi
    # The bare repo loom created is empty, so HEAD points at nothing; line it
    # up with Forgejo's default branch or every clone warns about a detached HEAD.
    if [ -n "$branch" ]; then
        git --git-dir "$LOOM_DATA/repos/$LOOM_OWNER/$name.git" symbolic-ref HEAD "refs/heads/$branch" 2>/dev/null || true
    fi
    refs=$(git -C "$tmp/$name.git" for-each-ref refs/heads refs/tags | wc -l)
    say "  ok    $target ($refs refs)"
    imported=$((imported + 1))
    printf '%s\t%s\n' "$name" "$private" >> "$tmp/done.tsv"
    rm -rf "$tmp/$name.git"
done < "$tmp/repos.tsv"

# ---- metadata (issues, releases) ----

# One python process per repo: JSON bodies with arbitrary text are painful in
# bash, and urllib is in the stdlib. Talks to both APIs directly.
meta_py() {
    python3 - "$@" <<'PY'
import json, sys, urllib.request, urllib.error

src, src_owner, repo, dst, dst_owner, token, what, src_token = sys.argv[1:9]
auth = {"Authorization": f"Bearer {token}"}

def get(url):
    req = urllib.request.Request(url, headers={"Authorization": f"token {src_token}"} if src_token else {})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)

def loom(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(dst + "/api/v1" + path, data=data, method=method,
                                 headers={**auth, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.load(e)
        except Exception:
            return e.code, {}

def paged(path):
    page = 1
    while True:
        part = get(f"{src}/api/v1{path}" + ("&" if "?" in path else "?") + f"limit=50&page={page}")
        if not part:
            return
        yield from part
        if len(part) < 50:
            return
        page += 1

if what == "issues":
    have = {l["name"].lower(): l for l in loom("GET", f"/repos/{dst_owner}/{repo}/labels")[1]}
    for lab in paged(f"/repos/{src_owner}/{repo}/labels"):
        if lab["name"].lower() not in have and lab.get("color", "#").startswith("#"):
            loom("POST", f"/repos/{dst_owner}/{repo}/labels",
                 {"name": lab["name"], "color": lab["color"], "description": lab.get("description") or ""})
    milestones = {}
    for ms in paged(f"/repos/{src_owner}/{repo}/milestones?state=all"):
        code, body = loom("POST", f"/repos/{dst_owner}/{repo}/milestones",
                          {"title": ms["title"], "description": ms.get("description") or ""})
        if code == 201:
            if ms.get("state") == "closed":
                loom("PATCH", f"/repos/{dst_owner}/{repo}/milestones/{body['id']}",
                     {"title": ms["title"], "closed": True})
            milestones[ms["title"].lower()] = ms["title"]
    n_issues = n_comments = 0
    for i in paged(f"/repos/{src_owner}/{repo}/issues?type=issues&state=all"):
        if "pull_request" in i and i["pull_request"] is not None:
            continue
        code, body = loom("POST", f"/repos/{dst_owner}/{repo}/issues", {"title": i["title"], "body": i.get("body") or ""})
        if code != 201:
            print(f"    issue #{i['number']}: loom said {code} {body.get('message', '')}", flush=True)
            continue
        num = body["number"]
        patch = {"state": "closed" if i.get("state") == "closed" else "open"}
        labels = [l["name"] for l in i.get("labels") or []]
        if labels:
            patch["labels"] = labels
        ms = (i.get("milestone") or {}).get("title")
        if ms and ms.lower() in milestones:
            patch["milestone"] = milestones[ms.lower()]
        loom("PATCH", f"/repos/{dst_owner}/{repo}/issues/{num}", patch)
        for c in get(f"{src}/api/v1/repos/{src_owner}/{repo}/issues/{i['number']}/comments"):
            loom("POST", f"/repos/{dst_owner}/{repo}/issues/{num}/comments", {"body": c.get("body") or ""})
            n_comments += 1
        n_issues += 1
        if i["number"] != num:
            print(f"    issue #{i['number']} -> #{num} ({i.get('state')})", flush=True)
        else:
            print(f"    issue #{num} ({i.get('state')})", flush=True)
    print(f"    {n_issues} issues, {n_comments} comments", flush=True)

if what == "releases":
    n = 0
    for rel in paged(f"/repos/{src_owner}/{repo}/releases"):
        code, body = loom("POST", f"/repos/{dst_owner}/{repo}/releases", {
            "tag_name": rel["tag_name"],
            "name": rel.get("name") or rel["tag_name"],
            "body": rel.get("body") or "",
            "draft": bool(rel.get("draft")),
            "prerelease": bool(rel.get("prerelease")),
        })
        if code == 201:
            n += 1
        elif code == 422:
            print(f"    release {rel['tag_name']}: no such tag here, skipped", flush=True)
        else:
            print(f"    release {rel['tag_name']}: loom said {code} {body.get('message', '')}", flush=True)
    print(f"    {n} releases", flush=True)
PY
}

import_issues() {
    local name=$1
    say "  issues for $LOOM_OWNER/$name:"
    meta_py "$FROM" "$OWNER" "$name" "$LOOM_URL" "$LOOM_OWNER" "$LOOM_TOKEN" issues "$TOKEN" || return 1
}

import_releases() {
    local name=$1
    say "  releases for $LOOM_OWNER/$name:"
    meta_py "$FROM" "$OWNER" "$name" "$LOOM_URL" "$LOOM_OWNER" "$LOOM_TOKEN" releases "$TOKEN" || return 1
}

if [ "$DO_ISSUES" -eq 1 ] || [ "$DO_RELEASES" -eq 1 ]; then
    if [ "$DRY" -eq 1 ]; then
        say ""
        say "dry run: no metadata was read"
    else
        [ -s "$tmp/done.tsv" ] || say "note: nothing imported, so nothing to copy metadata into"
        LOOM_TOKEN=$("$LOOM_BIN" token add "$LOOM_OWNER" --title forgejo-import 2>/dev/null) \
            || die "could not mint a loom API token for '$LOOM_OWNER' (does the user exist?)"
        [ "$DO_ISSUES" -eq 1 ] && say ""
        while IFS=$'\t' read -r name private; do
            [ "$DO_ISSUES" -eq 1 ] && { import_issues "$name" || true; }
            [ "$DO_RELEASES" -eq 1 ] && { import_releases "$name" || true; }
        done < "$tmp/done.tsv"
    fi
fi

say ""
say "imported $imported, skipped $skipped, failed $failed"
[ "$skipped" -gt 0 ] && say "note: existing repositories were left alone; pass --force to push into them again"
cat <<'NOTE'

Next steps:
  * pull requests, release assets, stars, topics and wikis are not imported.
NOTE
[ "$failed" -eq 0 ]
