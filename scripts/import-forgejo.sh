#!/usr/bin/env bash
# Import every repository of a Forgejo (or Gitea) user/org into loom.
#
# Run this on the machine that runs loom: repositories are registered with the
# CLI, which needs access to LOOM_DATA. Git data is copied with a mirror clone
# and a push of heads and tags, so every branch and tag arrives.
#
#   # public repositories, no credentials needed on the Forgejo side
#   ./scripts/import-forgejo.sh --from http://localhost:3030 --owner Fel-2 \
#       --loom-owner fel
#
#   # private ones too
#   ./scripts/import-forgejo.sh --from https://git.example.com --owner me \
#       --token <forgejo-api-token> --loom-owner fel
#
# What it does not do: issues, pull requests, releases, stars, topics and
# wikis have no API on the loom side, so they are not imported. Release pages
# have to be recreated per tag in the web UI (a tag alone is not a release).
set -uo pipefail

FROM=""
OWNER=""
TOKEN=""
LOOM_OWNER=""
LOOM_DATA="${LOOM_DATA:-$HOME/.local/share/loom}"
LOOM_BIN="${LOOM_BIN:-$(command -v loom || echo "$HOME/.local/bin/loom")}"
PUSH_BASE="${PUSH_BASE:-ssh://git@127.0.0.1:2223}"
DRY=0
FORCE=0

die() { echo "error: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'USAGE'

Options:
  --from URL         Forgejo base URL (http://host:port)
  --owner NAME       Forgejo user or org whose repositories to import
  --token TOKEN      Forgejo API token (needed for private repositories)
  --loom-owner NAME  loom user that will own them (default: --owner, lowercased)
  --loom-data DIR    loom data directory (default: $LOOM_DATA)
  --loom-bin PATH    loom binary (default: loom on PATH)
  --push-base URL    where to push (default: ssh://git@127.0.0.1:2223)
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
        --loom-data) LOOM_DATA=${2:-}; shift 2 ;;
        --loom-bin) LOOM_BIN=${2:-}; shift 2 ;;
        --push-base) PUSH_BASE=${2:-}; shift 2 ;;
        --dry-run) DRY=1; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

[ -n "$FROM" ] || die "--from is required"
[ -n "$OWNER" ] || die "--owner is required"
FROM=${FROM%/}
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
    rm -rf "$tmp/$name.git"
done < "$tmp/repos.tsv"

say ""
say "imported $imported, skipped $skipped, failed $failed"
[ "$skipped" -gt 0 ] && say "note: existing repositories were left alone; pass --force to push into them again"
cat <<'NOTE'

Next steps:
  * releases, issues and pull requests are not imported — there is no API for
    them. Recreate releases per tag in the web UI if you need them.
  * set LOOM_URL and LOOM_SSH_HOST in ~/.config/loom/env to the address other
    machines use, otherwise clone URLs say localhost.
NOTE
[ "$failed" -eq 0 ]
