#!/usr/bin/env bash
# Build a portable tarball: statically linked binary + installer + user unit.
# Needs podman (the image has the Rust toolchain); the result runs on any
# x86_64 Linux without a Rust install or matching glibc. Set PODMAN=... to
# use a different container engine that understands `build/create/cp/rm`.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=${ARCH:-x86_64}
NAME=loom-$VERSION-linux-$ARCH
OUT=dist/$NAME
rm -rf "$OUT" dist/$NAME.tar.gz
mkdir -p "$OUT/deploy"

PODMAN=${PODMAN:-podman}
command -v "$PODMAN" >/dev/null || { echo "need $PODMAN to build the portable binary (or set PODMAN=...)"; exit 1; }

echo "building static binary ($PODMAN)..."
"$PODMAN" build -q -f Dockerfile.dist -t "loom-dist:$VERSION" . >/dev/null
cid=$("$PODMAN" create "loom-dist:$VERSION")
"$PODMAN" cp "$cid:/src/target/release/loom" "$OUT/loom" >/dev/null
"$PODMAN" rm "$cid" >/dev/null
chmod 755 "$OUT/loom"
if command -v file >/dev/null; then file "$OUT/loom" | sed 's/^/  /'; fi
"$OUT/loom" version

cp deploy/loom-user.service "$OUT/deploy/loom.service"
cp deploy/loom-backup.service "$OUT/deploy/loom-backup.service"
cp deploy/loom-backup.timer "$OUT/deploy/loom-backup.timer"
cp deploy/install.sh "$OUT/deploy/install.sh"
cp README.md SPEC.md LICENSE "$OUT/" 2>/dev/null || true
cp -r docs "$OUT/" 2>/dev/null || true
cat > "$OUT/README-HOME.md" <<'DOC'
loom on a home laptop
=====================

  ./deploy/install.sh          # installs to ~/.local, enables a user service
  loom user add you --admin    # then create your account (asks for a password)
  loom repo create you/hello

Open http://<laptop-name>:3000 (or http://<laptop-ip>:3000) from your LAN.
The installer writes ~/.config/loom/env with LOOM_URL=http://<hostname>:3000;
edit it if you use a different name or port, then `systemctl --user restart loom`.

Clones:
  git clone http://<laptop>:3000/you/hello.git
  git clone ssh://git@<laptop>:2222/you/hello.git

Moving data from another machine (stop loom there first):
  loom backup /tmp/move --repos
  # copy /tmp/move to the laptop, then:
  LOOM_DATA=~/.local/share/loom loom restore /path/to/loom-<stamp>.db \
      --repos /path/to/repos-<stamp> --force
  systemctl --user restart loom

Low-memory tuning (old laptop). Put these in ~/.config/loom/env:
  LOOM_GIT_THREADS=2      # fewer git worker threads
  LOOM_MAX_GIT=2          # fewer concurrent clones/pushes
  LOOM_RATE_WRITE=40      # gentler write budget per IP

Nothing here needs root, a reverse proxy, or TLS: it is a LAN service.
DOC
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
echo
echo "made dist/$NAME.tar.gz ($(du -h "dist/$NAME.tar.gz" | cut -f1))"
