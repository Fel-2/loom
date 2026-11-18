#!/usr/bin/env bash
# Serve a throwaway loom instance publicly for a few hours, from this laptop,
# through a Cloudflare quick tunnel: no account, no open ports, no certificate,
# a random https://<words>.trycloudflare.com URL that dies with the process.
#
#   ./scripts/demo-local.sh [path/to/loom-project-*.tar.gz]
#
# Needs cloudflared on PATH:
#   curl -L -o ~/.local/bin/cloudflared \
#     https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64
#   chmod +x ~/.local/bin/cloudflared
#
# Two things this does that matter:
#   * the demo is a SEPARATE data directory — your real forge on :3000 is not
#     touched and stays on localhost;
#   * every account in the seeded data gets a random password, because the
#     bundle's are published in the repo. Anonymous visitors can browse and
#     clone; nobody can log in as admin and delete things.
#
# Ctrl-C stops loom and the tunnel.
set -uo pipefail

BUNDLE=${1:-}
PORT=${DEMO_PORT:-3100}
DATA=${DEMO_DATA:-$HOME/.local/share/loom-demo}
BIN=${LOOM_BIN:-$(command -v loom || echo "$HOME/.local/bin/loom")}

die() { echo "error: $*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

[ -x "$BIN" ] || die "no loom binary at $BIN (set LOOM_BIN=...)"
command -v cloudflared >/dev/null || die "cloudflared not on PATH — see the comment at the top of this script"

if [ ! -d "$DATA/repos" ]; then
    [ -n "$BUNDLE" ] || BUNDLE=$(ls -1 "$(dirname "$0")/../dist"/loom-project-*.tar.gz 2>/dev/null | tail -1)
    [ -f "$BUNDLE" ] || die "no seeded data at $DATA and no bundle given; pass dist/loom-project-0.2.0.tar.gz"
    say "seeding $DATA from $(basename "$BUNDLE")"
    tmp=$(mktemp -d)
    tar -C "$tmp" -xzf "$BUNDLE" || die "could not unpack $BUNDLE"
    mkdir -p "$DATA"
    cp -a "$tmp"/loom-project-*/test-data/. "$DATA"/
    rm -rf "$tmp"
    for u in alice bob git; do
        pw=$(head -c 48 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
        printf '%s\n' "$pw" | LOOM_DATA="$DATA" "$BIN" user passwd "$u" --password-stdin >/dev/null 2>&1 \
            || say "  (could not reset the password for $u)"
    done
    say "seeded; account passwords replaced with random values (not printed on purpose)"
fi

log=$(mktemp)
cleanup() { [ -n "${LOOM_PID:-}" ] && kill "$LOOM_PID" 2>/dev/null; [ -n "${CF_PID:-}" ] && kill "$CF_PID" 2>/dev/null; rm -f "$log"; }
trap cleanup EXIT INT TERM

say "opening a tunnel to 127.0.0.1:$PORT"
cloudflared tunnel --no-autoupdate --url "http://127.0.0.1:$PORT" >"$log" 2>&1 &
CF_PID=$!

url=""
for _ in $(seq 60); do
    url=$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' "$log" | head -1)
    [ -n "$url" ] && break
    sleep 0.5
done
[ -n "$url" ] || { tail -20 "$log"; die "the tunnel did not come up"; }

say "public URL: $url"
say "starting loom on 127.0.0.1:$PORT (real instance on :3000 is untouched)"

LOOM_DATA="$DATA" LOOM_ADDR="127.0.0.1:$PORT" LOOM_SSH_ADDR=off \
    LOOM_URL="$url" LOOM_TRUST_PROXY=1 LOOM_RATE_WRITE=20 \
    "$BIN" serve &
LOOM_PID=$!

for _ in $(seq 40); do
    curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break
    sleep 0.25
done
curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null || die "loom did not start; check the output above"

say ""
say "demo is live: $url"
say "clone URL:    $url/git/git.git"
say "Ctrl-C to stop (the URL dies with it)."
say ""
say "Keep the laptop awake meanwhile, in another terminal:"
say "  systemd-inhibit --what=idle:sleep --why='loom demo' sleep infinity"

wait "$LOOM_PID"
