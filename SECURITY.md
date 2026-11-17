# Security

loom runs a git server, an SSH server and a web application that stores code.
Please report problems.

## Reporting

Open a [private security advisory](https://github.com/Fel-2/loom/security/advisories/new)
or email the address on the maintainer's GitHub profile. Do not open a public
issue for anything exploitable. Expect an answer within a week; this is a
spare-time project, not a vendor.

## Threat model

loom is built for **one person or a small trusted group on a private network**.
That assumption is load-bearing:

- **No multi-tenant hardening.** Every signed-in user is untrusted, but the
  deployment is expected to be a handful of people who know each other. There is
  no per-org isolation of resources beyond repository permissions.
- **No audit log.** Actions are not recorded for later attribution.
- **No SSO, OIDC, LDAP or SCIM.** Accounts are local (argon2id) with optional
  TOTP; there is no way to disable password login centrally.
- **No federation.** Nothing is shared with other instances, so there is no
  remote-actor trust surface — and no protection for one if it is added.
- **TLS is your reverse proxy's job.** loom speaks plain HTTP. Put Caddy, nginx
  or similar in front and set `LOOM_URL` to the public `https://` URL, which is
  what makes session cookies `Secure`.
- **Rate limits are per IP, in memory.** They reset on restart and see nothing
  behind a proxy unless `LOOM_TRUST_PROXY=1` is set.

## What is in place

- A `Content-Security-Policy` on every response: `default-src 'none'`, with
  only the stylesheet, forms and images allowed (the UI ships no JavaScript at
  all; inline `style` attributes carry label colors and bar widths). Images may
  come from any host because READMEs and issues embed badges and screenshots
  elsewhere — `<img>` cannot execute script, so the cost is third-party
  requests, not XSS.
- argon2id password hashing, SHA-256-hashed session tokens, HttpOnly +
  SameSite=Lax cookies
- Cross-origin form posts rejected (`Origin` must match `Host`); every
  state-changing form is also a POST
- Per-repo authorisation on web, HTTP push and SSH, including private repos
  (covered by `tests/smoke.sh`)
- Raw HTML in markdown is rendered as text, and link destinations are limited to
  http, https, mailto, ftp and relative URLs
- Branch protection (no force-push, no delete, PRs only, required status checks)
  enforced in a `pre-receive` hook
- Signed webhooks (HMAC-SHA256), short-lived LFS tokens, per-IP write throttling
- No shell access over SSH: only `git-upload-pack`, `git-receive-pack` and
  `git-lfs-authenticate` are accepted

## Known gaps

- Not audited by anyone but the author, and not fuzzed.
- `loom user 2fa-off` is an unauthenticated-at-the-DB-level recovery path: it
  needs filesystem access to the data directory, so it protects against a lost
  authenticator, not against a compromised host.
- LFS objects and release assets are stored as files under `LOOM_DATA` with no
  per-object encryption.
