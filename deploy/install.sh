#!/bin/sh
# Install loom for the current user: ~/.local/bin + ~/.local/share/loom,
# plus a systemd user service. No root needed.
#   ./install.sh              install and start (with systemd user units)
#   ./install.sh --no-service install files only (start it yourself)
set -eu
SERVICE=1
[ "${1:-}" = "--no-service" ] && SERVICE=0
HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX=${PREFIX:-$HOME/.local}
BIN=$PREFIX/bin/loom
DATA=$PREFIX/share/loom
ENVF=$HOME/.config/loom/env
UNIT=$HOME/.config/systemd/user/loom.service
BUNIT=$HOME/.config/systemd/user/loom-backup.service
BTIMER=$HOME/.config/systemd/user/loom-backup.timer

command -v git >/dev/null || { echo "git is required: install it first"; exit 1; }
SRC=$HERE/../loom
[ -x "$SRC" ] || SRC=$HERE/../target/release/loom
[ -x "$SRC" ] || { echo "no loom binary: build it with 'cargo build --release' or unpack the dist tarball"; exit 1; }

mkdir -p "$PREFIX/bin" "$DATA" "$HOME/.config/loom" "$HOME/.config/systemd/user"
install -m 755 "$SRC" "$BIN"
install -m 644 "$HERE/loom.service" "$UNIT"
[ -f "$HERE/loom-backup.service" ] && install -m 644 "$HERE/loom-backup.service" "$BUNIT"
[ -f "$HERE/loom-backup.timer" ] && install -m 644 "$HERE/loom-backup.timer" "$BTIMER"
if [ ! -f "$ENVF" ]; then
    HOST=$(hostname 2>/dev/null || echo localhost)
    cat > "$ENVF" <<ENV
# loom environment (read by the systemd user service)
LOOM_URL=http://$HOST:3000
LOOM_SSH_HOST=$HOST:2222
# Old laptop? Uncomment to trade concurrency for memory:
#LOOM_GIT_THREADS=2
#LOOM_MAX_GIT=2
#LOOM_RATE_WRITE=40
ENV
    echo "wrote $ENVF (edit LOOM_URL if your host name differs)"
fi

if [ "$SERVICE" = 1 ] && command -v systemctl >/dev/null && systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user daemon-reload
    systemctl --user enable --now loom
    [ -f "$BTIMER" ] && systemctl --user enable --now loom-backup.timer
    loginctl enable-linger "$USER" 2>/dev/null || true
    echo "service: $(systemctl --user is-active loom) (starts at boot)"
else
    echo "files installed; run it by hand:"
    echo "  LOOM_DATA=$DATA $BIN serve"
fi

echo
echo "next: create your account"
echo "  LOOM_DATA=$DATA $BIN user add $(id -un) --admin"
echo "then open http://$(hostname 2>/dev/null || echo localhost):3000"
echo
echo "backups: daily, last 7 kept, in $DATA/backups (systemctl --user list-timers)"
