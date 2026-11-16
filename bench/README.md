# loom vs Forgejo — side-by-side benchmark

Same machine, same data, one server measured at a time while the other idles.

## What's compared

- **Forgejo 15.0.6** (the production version), Docker image, **SQLite**, defaults,
  no memory limit — `bench/forgejo/docker-compose.yml`. SQLite is Forgejo's
  leanest setup; the production deployment uses Postgres, which adds a database
  server on top of what's measured here.
- **loom** release build, `127.0.0.1:3917`.

Data, identical on both (verified by hashing `ls-remote` output):

- `git/git` — 83k commits, 1,027 branches+tags (from github.com/git/git)
- `alice/ytm-tui` — small repo
- 20 issues × 3 comments on `git/git`, created through each server's API
- PR #21 `bench-pr` → `master` (one commit, two files)

## What's measured

`bench/loadgen` (standalone Rust client, keep-alive HTTP/1.1):

1. **idle** RSS of the server process for 20 s
2. **uncontended latency**: 1 client, every page 10× (after 2 warm-up passes)
3. **load**: 32 concurrent clients, every page 40×, interleaved; peak RSS of the
   server and of its whole process tree (incl. spawned `git` children)
4. RSS 70 s after load
5. **clones**: 8× `ytm-tui` + 2× `git/git` concurrently over HTTP; time and peak
   process-tree RSS
6. RSS 70 s after clones

Pages (each mapped to the equivalent URL on the other server): repo home, tree,
blob (`diff.c`, 900 KB of C), raw file, commit log (first page and page 50),
commit, branches, issue list, issue, PR, PR files, small repo, user page, and
two API calls.

## Run it

```sh
# Forgejo
cd bench/forgejo && FORGEJO_BENCH_DATA=~/.cache/loom-bench/forgejo-data docker compose up -d
docker exec -u git forgejo-bench forgejo admin user create --admin \
  --username alice --password alice-test-pw --email alice@bench.local --must-change-password=false
# create org `git`, repos git/git + alice/ytm-tui via the API, push the same refs
# (refs/heads/* and refs/tags/*), create the 20 issues/comments + PR on both.

# loom must be running on :3917 with the same repos, then:
cargo build --release --manifest-path bench/loadgen/Cargo.toml
./bench/run.sh            # writes bench/results/<timestamp>.txt
```

Git always runs with `GIT_CONFIG_GLOBAL=/dev/null -c credential.helper=` so no
credential helper (or GUI prompt) is involved.

## Caveats

- RSS includes file-backed pages of each binary (loom ~13 MB on disk, Forgejo
  ~100 MB). Both are counted the same way.
- Page sizes differ a lot (Forgejo pages carry more markup/JS); latency includes
  transferring them over loopback.
- loom's "branches" page also lists tags; Forgejo's lists branches only.
- Forgejo's container also runs s6 and sshd; reported separately via `docker stats`.
- The client shares the CPU with the server being measured.
