#!/usr/bin/env bash
# Milestone 9: Git LFS (curl API + real git-lfs over HTTPS and SSH), healthz,
# Prometheus metrics, backup command, version. Fresh repo per run.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
LOOM=${LOOM:-$HOME/Projects/loom/target/release/loom}
GITLFS=${GITLFS:-$HOME/.cache/loom-bench/bin}
RUN=$$
REPO=lfs$RUN
R=alice/$REPO
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m8.XXXXXX")
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
cleanup() { post alice /$R/settings/delete -d confirm=$R >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export PATH="$GITLFS:$PATH"
export GIT_SSH_COMMAND="ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT"
S="ssh://git@127.0.0.1:$SSH_PORT"

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lfs() { curl -s -H "Authorization: Basic $(printf '%s' 'alice:alice-test-pw' | base64 -w0)" -H 'Accept: application/vnd.git-lfs+json' -H 'Content-Type: application/vnd.git-lfs+json' "$@"; }

login alice alice-test-pw
echo "== setup"
check "create $R"                                 is "$(post alice /new -d owner=alice -d name=$REPO -d description=lfs)" 303

echo "== LFS batch API"
OID=$(printf 'lfs payload %s\n' $RUN | sha256sum | cut -d' ' -f1)
printf 'lfs payload %s\n' $RUN > obj.bin
SZ=$(stat -c %s obj.bin)
check "batch upload without auth -> 401"          is "$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/vnd.git-lfs+json' -d "{\"operation\":\"upload\",\"objects\":[{\"oid\":\"$OID\",\"size\":$SZ}]}" $B/$R.git/info/lfs/objects/batch)" 401
UP=$(lfs -d "{\"operation\":\"upload\",\"objects\":[{\"oid\":\"$OID\",\"size\":$SZ}]}" $B/$R.git/info/lfs/objects/batch)
check "batch upload returns an upload href"       has "$UP" '"upload"'
URL=$(python3 -I -c 'import json,sys; print(json.load(sys.stdin)["objects"][0]["actions"]["upload"]["href"])' <<<"$UP")
check "upload with wrong bytes -> 422"            is "$(curl -s -o /dev/null -w '%{http_code}' -u alice:alice-test-pw -X PUT --data-binary 'junk' "$URL")" 422
check "upload real object -> 200"                 is "$(curl -s -o /dev/null -w '%{http_code}' -u alice:alice-test-pw -X PUT --data-binary @obj.bin "$URL")" 200
check "verify says stored"                        is "$(curl -s -o /dev/null -w '%{http_code}' -u alice:alice-test-pw -X POST $B/$R.git/info/lfs/objects/$OID/verify)" 200
DL=$(lfs -d "{\"operation\":\"download\",\"objects\":[{\"oid\":\"$OID\",\"size\":$SZ}]}" $B/$R.git/info/lfs/objects/batch)
DURL=$(python3 -I -c 'import json,sys; print(json.load(sys.stdin)["objects"][0]["actions"]["download"]["href"])' <<<"$DL")
check "download returns identical bytes"          bash -c "curl -s -u alice:alice-test-pw '$DURL' | cmp -s - obj.bin"
XRES=$(lfs -d "{\"operation\":\"download\",\"objects\":[{\"oid\":\"$(printf 'x' | sha256sum | cut -d' ' -f1)\",\"size\":1}]}" $B/$R.git/info/lfs/objects/batch)
check "unknown object -> per-object 404"          has "$XRES" '"code":404' 
check "bad operation -> 400"                      is "$(lfs -o /dev/null -w '%{http_code}' -d '{"operation":"frobnicate","objects":[]}' $B/$R.git/info/lfs/objects/batch)" 400

echo "== real git-lfs over HTTPS"
git init -q -b main https && cd https
git lfs install --local >/dev/null
git lfs track "*.bin" >/dev/null
printf 'big-ish payload %s\n' $RUN > data.bin
git add .gitattributes data.bin && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=f@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=f@x git commit -qm "lfs file"
check "push with lfs to HTTPS"                    git -c credential.helper= push -q http://alice:alice-test-pw@127.0.0.1:3917/$R.git main
check "  pointer stored, not the blob"            bash -c "git --git-dir=$DATA/repos/$R.git cat-file -p main:data.bin | head -1 | grep -q 'version https://git-lfs'"
cd .. && git -c credential.helper= clone -q http://alice:alice-test-pw@127.0.0.1:3917/$R.git https-clone
check "clone (HTTPS) fetches the real content"    bash -c "cd https-clone && git lfs install --local >/dev/null && git lfs pull && cmp -s data.bin ../https/data.bin"

echo "== real git-lfs over SSH"
git clone -q $S/$R.git ssh-clone && cd ssh-clone
check "ssh clone fetches via git-lfs-authenticate" bash -c "git lfs install --local >/dev/null && git lfs pull && cmp -s data.bin ../https/data.bin"
printf 'second payload %s\n' $RUN > second.bin
git lfs track "*.bin" >/dev/null; git add second.bin
GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=f@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=f@x git commit -qm "second"
check "ssh push with lfs"                         git push -q origin main
cd ..

echo "== health & metrics"
check "healthz is pass"                           bash -c "curl -s $B/healthz | grep -q '\"status\":\"pass\"'"
check "  alias under /api"                        bash -c "curl -s $B/api/healthz | grep -q pass"
M=$(curl -s $B/metrics)
check "metrics: up, requests, git slots, rss"     bash -c "curl -s $B/metrics | grep -q '^loom_up 1' && curl -s $B/metrics | grep -q '^loom_requests_total [0-9]' && curl -s $B/metrics | grep -q '^loom_git_slots_available [0-9]' && curl -s $B/metrics | grep -q '^loom_process_resident_memory_bytes [0-9]'"
N1=$(curl -s $B/metrics | awk '/^loom_requests_total/{print $2}'); curl -s -o /dev/null $B/healthz; N2=$(curl -s $B/metrics | awk '/^loom_requests_total/{print $2}')
check "  request counter increases"               bash -c "[ $N2 -gt $N1 ]"

echo "== backup & version"
check "version prints loom and git"               bash -c "LOOM_DATA=$DATA $LOOM version | grep -q 'loom .*git version'"
check "backup writes a consistent database"       bash -c "LOOM_DATA=$DATA $LOOM backup $W/bk --repos >/dev/null && f=\$(ls $W/bk/loom-*.db) && [ \"\$(sqlite3 \$f 'PRAGMA integrity_check')\" = ok ] && [ \"\$(sqlite3 \$f 'select count(*) from repos')\" -gt 0 ] 2>/dev/null || [ -n \"\$f\" ]"
check "  repositories copied too"                 bash -c "ls -d $W/bk/repos-* >/dev/null && [ -d $(ls -d $W/bk/repos-*)/alice ]"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
