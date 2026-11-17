#!/usr/bin/env bash
# Footprint guard: fail the build if loom outgrows its budget.
#
# Starts its own instance on a scratch data directory (nothing from the test
# suites is reused), pushes a small repo over HTTP with a token, hammers the
# hot pages, then asserts:
#   * no 5xx and no dropped connections during the load
#   * idle RSS after the load is under RSS_AFTER_MAX_MB
# The idle budget is the product: SPEC.md says loom idles under 20 MB; the
# guard allows headroom for CI runners and different malloc versions.
#
#   ./bench/guard.sh [loom-binary]
#
# Overrides: RSS_AFTER_MAX_MB (32)  SECS_LOAD (12)  PORT (3941)
set -u
BIN=${1:-$(cd "$(dirname "$0")/.." && pwd)/target/release/loom}
[ -x "$BIN" ] || { echo "no loom binary at $BIN (build it first)"; exit 1; }
command -v curl >/dev/null || { echo "curl is required"; exit 1; }

RSS_AFTER_MAX_MB=${RSS_AFTER_MAX_MB:-32}
SECS_LOAD=${SECS_LOAD:-12}
PORT=${PORT:-3941}
B=http://127.0.0.1:$PORT
W=$(mktemp -d "${TMPDIR:-/tmp}/loom-guard.XXXXXX")
SRV=
cleanup() { [ -n "$SRV" ] && kill "$SRV" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT

die() { echo "FAIL: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

export LOOM_DATA="$W/data" GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
printf 'guard-password-1\n' | "$BIN" user add guard --admin --password-stdin >/dev/null 2>&1
TOKEN=$("$BIN" token add guard --title guard) || die "could not mint a token"
"$BIN" repo create guard/pages --desc "footprint fixture" >/dev/null || die "could not create the repo"

git init -q -b main "$W/src"
for i in 1 2 3; do printf 'line %d\n' $i >> "$W/src/file.txt"; (cd "$W/src" && git add . && GIT_AUTHOR_NAME=g GIT_AUTHOR_EMAIL=g@g GIT_COMMITTER_NAME=g GIT_COMMITTER_EMAIL=g@g git commit -qm "commit $i"); done

LOOM_ADDR="127.0.0.1:$PORT" LOOM_SSH_ADDR=off LOOM_URL="$B" "$BIN" serve >/dev/null 2>&1 &
SRV=$!
for i in $(seq 60); do curl -sf "$B/healthz" >/dev/null && break; sleep 0.25; done
curl -sf "$B/healthz" >/dev/null || die "server did not come up"
git -C "$W/src" push -q "http://guard:$TOKEN@127.0.0.1:$PORT/guard/pages.git" main 2>/dev/null \
    || die "push with a token failed"

rss_kb() { awk '/VmRSS/ {print $2}' "/proc/$SRV/status" 2>/dev/null; }

idle0=$(rss_kb)
say "idle RSS before load: $((idle0 / 1024)) MB"

PAGES=("/guard/pages" "/guard/pages/tree/main" "/guard/pages/blob/main/file.txt" "/guard/pages/log/main"
       "/guard/pages/commit/main" "/guard/pages/refs" "/guard/pages/issues" "/guard/pages/labels"
       "/guard/pages/atom.xml" "/guard/pages/issues.atom.xml" "/guard/pages/releases.atom.xml"
       "/api/v1/repos/guard/pages" "/api/v1/repos/guard/pages/issues" "/healthz" "/metrics")
errors=0 requests=0
end=$((SECONDS + SECS_LOAD))
while [ $SECONDS -lt $end ]; do
    for p in "${PAGES[@]}"; do
        code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$B$p")
        requests=$((requests + 1))
        [ "$code" = 200 ] || { errors=$((errors + 1)); say "  $p -> $code"; }
    done
done
peak=$(rss_kb)
say "requests: $requests, errors: $errors"
say "peak RSS under load: $((peak / 1024)) MB"
[ "$errors" -eq 0 ] || die "$errors requests failed under load"

say "waiting 10 s for the heap to settle"
sleep 10
after=$(rss_kb)
say "idle RSS after load: $((after / 1024)) MB (budget ${RSS_AFTER_MAX_MB} MB)"
[ "$after" -le $((RSS_AFTER_MAX_MB * 1024)) ] || die "idle RSS after load is $((after / 1024)) MB, over the ${RSS_AFTER_MAX_MB} MB budget"

say "guard ok: $requests requests, $errors errors, $((after / 1024)) MB after load"
