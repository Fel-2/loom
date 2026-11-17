# Running loom on a home laptop

A complete guide for a personal, LAN-only instance: no root, no TLS, no
reverse proxy, no external services. Idle cost is around **17 MB of RAM and
one thread**, so a 10-year-old laptop is plenty.

- [1. What you need](#1-what-you-need)
- [2. Get the binary onto the laptop](#2-get-the-binary-onto-the-laptop)
- [3. Install](#3-install)
- [4. Your first account and repository](#4-your-first-account-and-repository)
- [5. Reaching it from other devices](#5-reaching-it-from-other-devices)
- [6. Day-to-day](#6-day-to-day)
- [7. Backups](#7-backups)
- [8. Moving an instance to another machine](#8-moving-an-instance-to-another-machine)
- [9. Upgrading](#9-upgrading)
- [10. Tuning for a slow laptop](#10-tuning-for-a-slow-laptop)
- [11. Security on a home network](#11-security-on-a-home-network)
- [12. Troubleshooting](#12-troubleshooting)
- [13. Where everything lives](#13-where-everything-lives)
- [14. Removing it](#14-removing-it)
- [15. Known limits](#15-known-limits)

## 1. What you need

- Any x86_64 Linux with `git` installed (`apt install git`,
  `pacman -S git`, …). systemd is nice to have; without it you can run
  loom by hand or from a service manager of your choice.
- About 50 MB of disk for the binary and an empty instance, plus whatever
  your repositories need. A backup set costs about the size of `data/repos`
  (see [Backups](#7-backups)).
- No root: everything installs under your home directory.

## 2. Get the binary onto the laptop

### Option A — portable tarball from another machine (recommended)

On the machine where you develop loom:

```sh
./scripts/dist.sh
# -> dist/loom-0.1.0-linux-x86_64.tar.gz  (statically linked, ~14 MB)
```

The binary is built for musl in a container (podman by default), so it does
not care which glibc the laptop has. Copy it over:

```sh
scp dist/loom-0.1.0-linux-x86_64.tar.gz laptop:~
# or a USB stick
ssh laptop 'tar xzf loom-0.1.0-linux-x86_64.tar.gz'
```

### Option B — build on the laptop

Needs Rust (`rustup`) and ~1 GB of RAM for the build:

```sh
git clone <your copy of this repo> loom && cd loom
cargo build --release
```

`./scripts/dist.sh` also works there if podman is installed (or set
`PODMAN=docker`).

### Option C — container (podman)

The repository's `Dockerfile` and `compose.yml` work with podman:

```sh
podman build -t loom .
mkdir -p data
podman run -d --name loom -p 3000:3000 -p 2222:2222 \
  -e LOOM_URL=http://mylaptop:3000 -e LOOM_SSH_HOST=mylaptop:2222 \
  -v ./data:/data:Z loom
# or: podman-compose up -d   (or `podman compose up -d`)
```

Rootless podman maps the container's uid 10001 to a subuid, so `./data` ends
up owned by that mapping. To keep the files readable by your user, run with
`--userns=keep-id` and a `data` directory owned by you, or accept the mapping
and use `podman unshare` when you need to look inside.

This is heavier than a plain binary; for an old laptop prefer option A.

## 3. Install

```sh
cd loom-0.1.0-linux-x86_64
./deploy/install.sh
```

What it does, all as your user:

| Item | Location |
|---|---|
| Binary | `~/.local/bin/loom` |
| Data (database, repos, LFS, releases) | `~/.local/share/loom` |
| Environment file | `~/.config/loom/env` |
| systemd user unit | `~/.config/systemd/user/loom.service` |
| Daily backup timer | `~/.config/systemd/user/loom-backup.timer` |

It also runs `loginctl enable-linger "$USER"` so the service keeps running
after you log out, and starts loom on **port 3000** (HTTP) and **2222** (SSH).
Use `./deploy/install.sh --no-service` to only install files (then start it
yourself, or hand the units to another service manager).

Do it by hand if you prefer:

```sh
mkdir -p ~/.local/bin ~/.local/share/loom
cp loom ~/.local/bin/
cat > ~/.config/loom/env <<'ENV'
LOOM_URL=http://mylaptop:3000
LOOM_SSH_HOST=mylaptop:2222
ENV
LOOM_DATA=~/.local/share/loom LOOM_ADDR=0.0.0.0:3000 \
  LOOM_SSH_ADDR=0.0.0.0:2222 ~/.local/bin/loom serve
```

Either way, `loom doctor` tells you whether the machine is ready (git
present, data directory writable, ports free, enough disk):

```sh
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom doctor
```

### runit (Artix, Void, and other non-systemd machines)

`deploy/runit/` holds a run script and a `conf`. It runs as a real user, so
sudo to install it:

```sh
sudo mkdir -p /etc/runit/sv/loom
sudo cp deploy/runit/run /etc/runit/sv/loom/run
sudo chmod +x /etc/runit/sv/loom/run
sudo sh -c 'cat > /etc/runit/sv/loom/conf <<EOF
LOOM_USER=you
LOOM_BIN=/home/you/.local/bin/loom
LOOM_DATA=/home/you/.local/share/loom
EOF'
sudo ln -s /etc/runit/sv/loom /etc/runit/runsvdir/default/loom
sudo sv status loom && sudo sv restart loom
```

The script sources the user's `~/.config/loom/env` afterwards, so listener
addresses are configured in one place for both runit and manual runs. Logs go
to runit's `svlogd`, or `ln -s /var/log/sv/loom ...` if you want a log dir.

## 4. Your first account and repository

```sh
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom user add you --admin
# asks for a password (8+ characters); or pipe it:
# printf 'a long password' | ... user add you --admin --password-stdin

LOOM_DATA=~/.local/share/loom ~/.local/bin/loom repo create you/hello \
  --desc "first repository"
```

Open <http://mylaptop:3000>, sign in, and try the clone box on the repo page.
Create more users the same way (`loom user add friend`), and let them add an
SSH key under **Settings → ssh keys**.

Pushing:

```sh
cd ~/some/project
git init -b main && git add . && git commit -m "first"
git remote add origin http://mylaptop:3000/you/hello.git
git push -u origin main
```

Over SSH (no password once your key is registered):

```sh
git remote add origin ssh://git@mylaptop:2222/you/hello.git
```

## 5. Reaching it from other devices

The service listens on all interfaces. From another machine on the LAN:
`http://<laptop-ip>:3000`, or `http://<laptop-name>.local:3000` if your router
does mDNS. Find the address with `hostname -I`.

If the laptop has a firewall, open the two ports:

```sh
# ufw
sudo ufw allow 3000/tcp && sudo ufw allow 2222/tcp
# firewalld
sudo firewall-cmd --add-port=3000/tcp --add-port=2222/tcp --permanent && sudo firewall-cmd --reload
```

**Set `LOOM_URL` to the address other devices use** (in
`~/.config/loom/env`), otherwise clone URLs and webhook payloads contain
`127.0.0.1`. Same for `LOOM_SSH_HOST`. Then `systemctl --user restart loom`.

## 6. Day-to-day

```sh
systemctl --user status loom          # is it running?
systemctl --user restart loom         # after changing ~/.config/loom/env
journalctl --user -u loom -f          # logs
systemctl --user stop loom
```

Anything the CLI does (users, keys, repos, backup) can run while the server
is up — SQLite is in WAL mode and the CLI opens its own connection.

Updating a user's password: `loom user passwd alice`.
Lost your authenticator? `loom user 2fa-off you`.

## 7. Backups

`loom backup` writes a consistent snapshot of the database and (with
`--repos`) a copy of the repositories:

```sh
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom backup ~/loom-backups --repos --keep 7
```

- `--keep 7` rotates old sets (a `loom-<stamp>.db` plus `repos-<stamp>`) and
  keeps the newest 7. Useful on a small disk.
- The database snapshot is taken with SQLite's `VACUUM INTO`, so it is
  consistent even while loom is running.
- The repository copy uses `cp -a --reflink=auto`: instant and free on
  btrfs/XFS, a full copy on ext4. It is **not** atomic: a push happening
  exactly during the copy could be half-copied. For a home instance that is
  usually fine; for a byte-perfect copy, `systemctl --user stop loom` first.
- Restoring needs the server stopped:

```sh
systemctl --user stop loom
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom restore \
  ~/loom-backups/loom-<stamp>.db --repos ~/loom-backups/repos-<stamp> --force
systemctl --user start loom
```

`loom restore` verifies the backup opens and reports how many users and
repositories it contains before replacing anything, and refuses without
`--force` if a database already exists.

The installer also enables **`loom-backup.timer`**: daily, into
`~/.local/share/loom/backups`, keeping 7 sets. Check it with
`systemctl --user list-timers loom-backup.timer`.

Backups live on the same disk unless you point `loom backup` somewhere else —
copy that directory off the laptop occasionally.

## 8. Moving an instance to another machine

On the old machine (laptop A):

```sh
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom backup /tmp/move --repos
```

Copy `/tmp/move` to laptop B (`scp -r`, USB, …), then there:

```sh
./deploy/install.sh                  # if loom isn't installed yet
systemctl --user stop loom 2>/dev/null || true
LOOM_DATA=~/.local/share/loom ~/.local/bin/loom restore \
  /path/to/move/loom-<stamp>.db --repos /path/to/move/repos-<stamp> --force
systemctl --user start loom
```

Update `LOOM_URL` / `LOOM_SSH_HOST` in `~/.config/loom/env` to the new
machine's name and restart. Nothing else changes: sessions, API tokens, SSH
keys, issues, PRs, releases, LFS objects and webhooks all live in `data/`.

If you prefer, stop the service on A and copy `~/.local/share/loom`
directly — same result, minus the consistency guarantees.

### Coming from Forgejo or Gitea

`scripts/import-forgejo.sh` imports every repository of a user or org, with all
branches and tags. Run it on the loom machine, because repositories are
registered with the CLI:

```sh
# public repositories
./scripts/import-forgejo.sh --from http://localhost:3030 --owner Fel-2 \
    --loom-owner fel --loom-data ~/.local/share/loom \
    --push-base ssh://git@localhost:2223

# private ones too: Forgejo → Settings → Applications → Generate token
./scripts/import-forgejo.sh --from https://git.example.com --owner me \
    --token <forgejo-token> --loom-owner fel
```

`--dry-run` lists what it would do. Existing repositories are skipped, so it is
safe to re-run; `--force` re-pushes into them. Descriptions, privacy and the
default branch are carried over.

What does not come across: issues, pull requests, releases, stars, topics and
wikis. loom has no API for them, so releases have to be recreated per tag in the
web UI — a git tag alone is not a release page.

## 9. Upgrading

Replace the binary and restart:

```sh
systemctl --user stop loom
cp new-loom ~/.local/bin/loom
systemctl --user start loom
```

Database migrations run automatically at startup (each schema version is
applied once, inside a transaction). Git hooks call the binary by its path,
resolved at startup, so replacing it while running is safe too — but restart
to pick up the new version. After a git upgrade, `loom repo maintain`
rebuilds commit-graphs.

## 10. Tuning for a slow laptop

Put any of these in `~/.config/loom/env` and restart:

| Variable | Default | Notes |
|---|---|---|
| `LOOM_GIT_THREADS` | `4` | Blocking threads for git work. 2 keeps memory lower under load. |
| `LOOM_MAX_GIT` | `8` | Concurrent clones/pushes. 2 is plenty for a household. |
| `LOOM_RATE_WRITE` | `100` | Writes per second per IP; lower on a weak CPU. |
| `LOOM_RATE_READ` | `2000` | Reads per second per IP. |
| `LOOM_LFS_MAX_MB` | `2048` | Cap for a single LFS object. |

Reality check: idle is ~17 MB RSS and one thread; the binary is ~14 MB and
mostly page cache. The expensive moments are clones of huge repositories
(the `git` child processes, not loom) and tree views with last-commit badges
enabled (per-repo setting, off by default — leave it off on an old machine).

## 11. Security on a home network

loom has no TLS of its own. On a trusted LAN that is a reasonable trade-off;
on shared Wi-Fi, put it behind a reverse proxy (Caddy/nginx) or a VPN, and:

- **Change the passwords you created while testing.** `loom user passwd`.
- Turn on **two-factor** (Settings → two-factor) for the admin account.
- Use **API tokens** instead of passwords for scripts; revoke them when done.
- Private repositories are enforced server-side, but anyone on the LAN can
  reach the login page; rate limiting protects against password guessing.
- If you put a proxy in front, set `LOOM_TRUST_PROXY=1` so login throttling
  sees real client addresses (only do this when a proxy you control sets
  `X-Forwarded-For`).
- Do not port-forward 3000/2222 to the internet as-is. If you need remote
  access, use WireGuard/Tailscale, or a TLS proxy with `LOOM_URL=https://…`
  (sessions then also get the `Secure` cookie flag).

## 12. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `bind 0.0.0.0:3000: address in use` | Something else uses the port. Change `LOOM_ADDR` (e.g. `:3001`) and `LOOM_URL`. |
| `git not found in PATH` at startup | Install git. The systemd user service inherits a minimal PATH; `/usr/bin/git` is normally enough. |
| Service stops when I log out | `loginctl enable-linger "$USER"` (the installer does this). |
| Clone URL says `127.0.0.1` on another device | Set `LOOM_URL` / `LOOM_SSH_HOST` to the laptop's LAN name and restart. |
| `Too many failed attempts` on login | Throttle: 5 failures per user from your IP, 15 minutes. Restart loom or wait. |
| `repository not found` when pushing | Check owner/name spelling; private repos need credentials (`http://user:token@…`). |
| 2FA codes rejected | Laptop clock drift; enable NTP (`timedatectl set-ntp true`). Codes allow ±30 s. |
| LFS push hangs or 401s | Check `git lfs version` on the client; over SSH the server answers `git-lfs-authenticate` itself. `loom lfs gc --dry-run` shows storage. |
| Disk full | `loom backup --keep 3`, `loom lfs gc`, and `data/trash` holds deleted repos (delete it if you don't want them back). |
| Nothing in `journalctl` | `journalctl --user -u loom` (user unit), not the system journal. |

`loom doctor` covers the first few automatically.

## 13. Where everything lives

```
~/.local/bin/loom                     binary
~/.local/share/loom/
  loom.db                             users, repos, issues, PRs, settings (SQLite/WAL)
  repos/<owner>/<name>.git            bare git repositories
  lfs/<repo id>/<aa>/<bb>/<oid>       Git LFS objects
  releases/<repo id>/<release id>/    release assets
  hooks/                              pre-receive/post-receive (call `loom hook …`)
  secret                              HMAC key for short-lived LFS tokens (0600)
  ssh_host_ed25519_key                SSH host key (generated on first start, 0600)
  trash/                              deleted repositories (recoverable)
  backups/                            created by the backup timer
~/.config/loom/env                    environment for the service
~/.config/systemd/user/loom.service   service
~/.config/systemd/user/loom-backup.*  daily backup timer
```

## 14. Removing it

```sh
systemctl --user disable --now loom loom-backup.timer
rm ~/.config/systemd/user/loom.service ~/.config/systemd/user/loom-backup.*
rm ~/.config/loom/env
rm ~/.local/bin/loom
rm -rf ~/.local/share/loom      # this deletes all repositories and data
```

Take a backup first if you might want the data later.

## 15. Known limits

- One machine, one process: no clustering, no replication.
- No built-in TLS — use a proxy or a VPN for anything beyond your LAN.
- Email notifications need an MTA: set `LOOM_MAIL_CMD` (e.g.
  `/usr/sbin/sendmail -t`); loom queues messages and hands them over.
- No OIDC/LDAP: accounts are local. That keeps the attack surface small.
- Per-file "last commit" badges are opt-in per repo because they are slow on
  large directories.
