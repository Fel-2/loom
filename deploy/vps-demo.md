# A demo instance on a small VPS, with no open ports

This is the setup loom is demoed with: one small VPS, nothing listening on the
public internet, Cloudflare in front for TLS, and Tailscale for getting in.
It fits in 2 GB of RAM with room to spare (loom is ~13 MB, cloudflared ~50 MB,
tailscaled ~40 MB).

Roughly €3-5/month. Works on any provider; Netcup VPS nano and OVH VPS-1 are
the two cheapest shapes that have this much headroom.

```
internet ─► Cloudflare edge ─► (outbound tunnel) ─► cloudflared ─► 127.0.0.1:3000  loom demo
                     │
                     └─ Access (email OTP) on admin.example.com only
your devices ─► Tailscale ─► tailscaled on the box (SSH, no public 22)
```

## 1. Base system

Debian 12 or 13, x86_64 (the release tarball matches). At the provider: add your
SSH key, no password login.

```sh
# as root, once
apt update && apt -y upgrade
apt -y install curl ca-certificates ufw unattended-upgrades restic
dpkg-reconfigure -plow unattended-upgrades     # or: echo 'Unattended-Upgrade::Automatic-Reboot "false";' > /etc/apt/apt.conf.d/51noreboot

# nothing should be reachable from outside: no 22, no 80, no 443
ufw default deny incoming
ufw default allow outgoing
ufw enable
```

Keep SSH reachable for now; it gets closed in step 3 once Tailscale works.
Locking yourself out before then is the classic way to spend an evening on a
rescue console.

## 2. Cloudflare tunnel (no inbound ports)

```sh
install -d -m 0755 /usr/share/keyrings
curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg \
  | tee /usr/share/keyrings/cloudflare-main.gpg >/dev/null
echo "deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main" \
  | tee /etc/apt/sources.list.d/cloudflared.list
apt update && apt -y install cloudflared

cloudflared tunnel login          # prints a URL; open it, pick your zone
cloudflared tunnel create loom    # note the UUID it prints
```

`/etc/cloudflared/config.yml`:

```yaml
tunnel: <UUID>
credentials-file: /root/.cloudflared/<UUID>.json
ingress:
  - hostname: demo.example.com
    service: http://127.0.0.1:3000
  - hostname: admin.example.com
    service: http://127.0.0.1:3001
  - service: http_status:404
```

```sh
cloudflared tunnel route dns loom demo.example.com
cloudflared tunnel route dns loom admin.example.com
systemctl enable --now cloudflared
```

## 3. Tailscale for access, and close SSH

```sh
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up --ssh --advertise-exit-node     # --ssh: no sshd needed; exit node: optional VPN
```

Confirm `tailscale ssh root@<host>` works from your laptop, then close the
public port:

```sh
ufw delete allow 22/tcp 2>/dev/null || true
ufw status                                # should show no allow rules
```

From here `ss -ltn` will still show tailscaled on its own interface and
cloudflared on none at all. Nothing is on `0.0.0.0`.

## 4. loom

```sh
adduser --system --group --home /var/lib/loom loom
cd /tmp
VER=0.2.0
curl -LO "https://github.com/Fel-2/loom/releases/download/v$VER/loom-$VER-linux-x86_64.tar.gz"
tar xzf "loom-$VER-linux-x86_64.tar.gz"
install -m 755 "loom-$VER-linux-x86_64/loom" /usr/local/bin/loom

install -d -o loom -g loom /var/lib/loom/demo
install -m 644 "loom-$VER-linux-x86_64/deploy/loom.service" /etc/systemd/system/loom-demo.service
```

Edit `/etc/systemd/system/loom-demo.service`: set `User=loom`, `Group=loom`,
`Environment=LOOM_DATA=/var/lib/loom/demo`, and add:

```
Environment=LOOM_ADDR=127.0.0.1:3000
Environment=LOOM_SSH_ADDR=off
Environment=LOOM_URL=https://demo.example.com
Environment=LOOM_TRUST_PROXY=1
Environment=LOOM_RATE_WRITE=20
```

`LOOM_URL` must be the public https URL or every clone link says localhost.
`LOOM_TRUST_PROXY=1` makes throttling read `X-Forwarded-For`; without it every
visitor shares Cloudflare's IP and one bucket.

## 5. Seed it with throwaway data

Never put your real instance on a public demo — accounts on loom are created by
the CLI, so anonymous visitors can read but cannot register, push or file
issues.

```sh
cd /tmp
curl -LO https://github.com/Fel-2/loom/releases/download/v0.2.0/loom-project-0.2.0.tar.gz
tar xzf loom-project-0.2.0.tar.gz
cp -a loom-project-0.2.0/test-data/. /var/lib/loom/demo/
chown -R loom:loom /var/lib/loom/demo

systemctl daemon-reload
systemctl enable --now loom-demo
curl -s localhost:3000/healthz
```

The bundle's accounts are `alice` (admin) / `bob`, passwords in its
`TESTING.md` — change them or leave them public on purpose; they're fake.

## 6. Cloudflare Access on the private hostname only

Zero Trust → Access → Applications:

- **`admin.example.com`** — an Access application, policy "allow: emails = you",
  one-time PIN. This is where your own data lives (port 3001, second instance).
- **`demo.example.com`** — *no* Access application, or one with a Bypass policy.
  Access requires a login; a demo nobody can open is not a demo.

In the zone's settings for the demo hostname, leave **Bot Fight Mode off** —
`git clone` cannot solve a JS challenge, and you'll get mystery 403s.

## 7. Backups and upgrades

```sh
# /etc/cron.daily/loom-backup
LOOM_DATA=/var/lib/loom/demo /usr/local/bin/loom backup /var/lib/backups --repos --keep 7
restic -r b2:bucket:path backup /var/lib/backups /var/lib/loom
```

Take a **provider snapshot** before every loom upgrade — an upgrade is a binary
swap plus an in-place SQLite migration, so rolling back is "snapshot, not
downgrade".

## 8. Checklist

```sh
ss -ltn                      # nothing on 0.0.0.0 or ::
ufw status                   # no allow rules
curl -sI https://demo.example.com | head -1     # 200, https, valid cert
git clone https://demo.example.com/git/git.git  # works anonymously
curl -sI https://admin.example.com | head -1    # 302 to Cloudflare Access
```

If those four pass, the box has no reachable attack surface beyond Cloudflare
and whatever loom itself does — and `SECURITY.md` explains what that is.
