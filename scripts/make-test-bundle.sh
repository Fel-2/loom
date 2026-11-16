#!/usr/bin/env bash
# Build a self-contained project + test-instance bundle:
#   source (with scrubbed history), static binary, seeded LOOM_DATA, test keys,
#   and TESTING.md with credentials and commands.
# The fixture repositories are synthetic: no personal history is included.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "warning: uncommitted changes are not part of the bundle (it clones HEAD)"
fi
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NAME=loom-project-$VERSION
OUT=$ROOT/dist/$NAME
BIN=$(ls dist/loom-*-linux-*/loom 2>/dev/null | head -1 || true)
[ -n "$BIN" ] || BIN=target/release/loom
[ -x "$BIN" ] || { echo "build first: ./scripts/dist.sh (or cargo build --release)"; exit 1; }
DATA=$OUT/test-data
KEYS=$OUT/test-keys
rm -rf "$OUT" "$ROOT/dist/$NAME.tar.gz"
mkdir -p "$OUT" "$DATA" "$KEYS"

echo "== source (with history, no target/data/dist)"
git clone -q --no-hardlinks "$ROOT" "$OUT/loom-source"
rm -rf "$OUT/loom-source/target" "$OUT/loom-source/data" "$OUT/loom-source/dist"
git -C "$OUT/loom-source" remote remove origin
# Reflogs record the clone source path; drop them so no local paths travel.
git -C "$OUT/loom-source" reflog expire --expire=now --all
rm -rf "$OUT/loom-source/.git/logs"
git -C "$OUT/loom-source" gc --prune=now -q

echo "== binary"
cp "$BIN" "$OUT/loom"
chmod 755 "$OUT/loom"

echo "== users"
export LOOM_DATA=$DATA LOOM_ADDR=127.0.0.1:3929 LOOM_SSH_ADDR=127.0.0.1:2929
pw() { "$OUT/loom" user add "$1" --password-stdin ${2:-} ; }
printf 'alice-test-pw' | pw alice --admin
printf 'bob-test-pw1' | pw bob
printf 'git-test-pw' | pw git
for r in alice/ytm-tui alice/slate alice/tokmon bob/bobs git/git; do
    "$OUT/loom" repo create "$r" --desc "test fixture" >/dev/null
done
"$OUT/loom" repo create alice/secret --private --desc "private fixture" >/dev/null

echo "== synthetic repositories"
seed() { # seed <owner/name> <files...>
    local full=$1; shift
    local w; w=$(mktemp -d)
    git init -q -b main "$w"
    for spec in "$@"; do
        local path=${spec%%:*} body=${spec#*:}
        mkdir -p "$w/$(dirname "$path")" 2>/dev/null || true
        printf '%s\n' "$body" > "$w/$path"
    done
    git -C "$w" add -A
    GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=local@local.com \
    GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=local@local.com \
        git -C "$w" commit -qm "initial commit"
    git -C "$w" tag v1.0
    GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=local@local.com \
    GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=local@local.com \
        git -C "$w" commit -q --allow-empty -m "second commit"
    git -C "$w" push -q "$DATA/repos/$full.git" main --tags
    rm -rf "$w"
}
seed alice/ytm-tui 'README.md:# ytm-tui (fixture)' 'src/main.rs:fn main() { println!("hi"); }' 'Cargo.toml:[package]
name = "ytm-tui"'
seed alice/slate 'README.md:# slate (fixture)' 'notes.txt:some notes'
seed alice/tokmon 'README.md:# tokmon (fixture)'
# bob/bobs stays empty on purpose: tests exercise the first push + HEAD fixup.
# git/git gets a few branches and tags so clone/log/search have something to chew.
seed git/git 'README.md:# git (fixture for tests)' 'diff.c:/* fixture */' 'Makefile:all:'
w=$(mktemp -d); git init -q -b master "$w"; echo x > "$w/x"; git -C "$w" add -A
GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=local@local.com GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=local@local.com git -C "$w" commit -qm "master fixture"
git -C "$w" branch feature/one; git -C "$w" branch feature/two
git -C "$w" push -q "$DATA/repos/git/git.git" 'refs/heads/*:refs/heads/*' --tags
rm -rf "$w"
git --git-dir="$DATA/repos/git/git.git" symbolic-ref HEAD refs/heads/master

echo "== ssh keys (test-only)"
for u in alice bob; do
    ssh-keygen -q -t ed25519 -N '' -C "$u-test" -f "$KEYS/$u"
    "$OUT/loom" key add "$u" "$KEYS/$u.pub" "$u test key" >/dev/null
    chmod 600 "$KEYS/$u"
done

echo "== instructions"
cat > "$OUT/TESTING.md" <<'DOC'
# loom — project + test instance bundle

Everything needed to run loom and its test suites on one machine, offline.

## Contents

| Path | What |
|---|---|
| `loom` | statically linked binary (no Rust, no glibc dependency) |
| `loom-source/` | full source **with git history** (identity: `fel <local@local.com>`) |
| `test-data/` | a seeded instance: SQLite DB, fixture repositories, LFS/release dirs |
| `test-keys/` | SSH keypairs for `alice` and `bob` (test only, already registered) |
| `TESTING.md` | this file |

The four fixture repositories (`alice/ytm-tui`, `alice/slate`, `alice/tokmon`,
`bob/bobs`) and `git/git` contain **synthetic** commits — no personal history.
`alice/secret` is a private repository, empty on purpose.

## Accounts

| User | Password | Role |
|---|---|---|
| `alice` | `alice-test-pw` | admin |
| `bob` | `bob-test-pw1` | normal user |
| `git` | `git-test-pw` | owner of the `git/git` fixture |

These are throwaway passwords for a local test instance. Do not reuse them
anywhere real. SSH access uses the bundled keys, not passwords.

## Run it

```sh
./loom doctor                         # sanity: git present, ports, disk
LOOM_DATA=$PWD/test-data \
LOOM_ADDR=127.0.0.1:3917 \
LOOM_SSH_ADDR=127.0.0.1:2917 \
LOOM_URL=http://127.0.0.1:3917 \
LOOM_SSH_HOST=127.0.0.1:2917 \
./loom serve
```

Then open <http://127.0.0.1:3917> and sign in as `alice` / `alice-test-pw`.

Clone something:

```sh
git clone http://127.0.0.1:3917/alice/ytm-tui.git
GIT_SSH_COMMAND="ssh -i $PWD/test-keys/alice -o IdentitiesOnly=yes -p 2917" \
  git clone ssh://git@127.0.0.1:2917/alice/ytm-tui.git
```

## Run the test suites

The suites mutate the instance (they create and delete repositories, issues,
tokens). Work on a copy if you want the seeded state intact:

```sh
cp -a test-data test-data.run
export LOOM=$PWD/loom
export LOOM_DATA=$PWD/test-data.run
export LOOM_TEST_URL=http://127.0.0.1:3917
export LOOM_TEST_SSH_PORT=2917
export KEYS=$PWD/test-keys
export PATH=$PWD:$PATH          # so `git lfs` tests can find git-lfs, if installed

# start the server against test-data.run in another terminal, then:
cd loom-source && ./tests/smoke.sh
./tests/m3.sh && ./tests/m4.sh && ./tests/m5.sh
./tests/m6.sh && ./tests/m7.sh && ./tests/m8.sh
./tests/m9.sh && ./tests/m10.sh
cargo test --release            # unit tests
```

Notes:

- Tests run git with `GIT_CONFIG_GLOBAL=/dev/null`, so your personal git config
  and credential helpers are never touched.
- `m8.sh` (Git LFS) wants `git-lfs` on `PATH`; without it those checks are
  skipped/fail. The server side needs nothing extra.
- `m10.sh` checks the static tarball only if `dist/` exists.
- The platform may already use ports 3000/2222; this bundle uses 3917/2917 as
  above (the scripts default to those).
- `bench/` is not included here beyond the scripts in the source; the Forgejo
  comparison needs its own setup.

## What is *not* included

- No real repositories, no personal git history.
- No TLS or reverse proxy; this is a local test instance.
- No backups directory (use `./loom backup DIR --repos` to make one).

## Starting over

Delete `test-data.run` (or `test-data`) and re-run the seeding steps, or restore
the pristine `test-data` if you kept a copy.
DOC

echo "== leak check"
# Split so this script itself doesn't contain the strings it forbids.
BAD1=$(printf 'mar%s' 'jan'); BAD2=$(printf 'jon%s' 'gis')
if grep -rIl -e "$BAD1" -e "$BAD2" "$OUT" 2>/dev/null | grep -v 'Binary file'; then
    echo "REFUSING: identity strings found above"; exit 1
fi
if git -C "$OUT/loom-source" log --all -p | grep -qi -e "$BAD1" -e "$BAD2"; then
    echo "REFUSING: identity strings in git history"; exit 1
fi

echo "== tarball"
tar -C "$ROOT/dist" -czf "$ROOT/dist/$NAME.tar.gz" "$NAME"
echo "made dist/$NAME.tar.gz ($(du -h "$ROOT/dist/$NAME.tar.gz" | cut -f1))"
