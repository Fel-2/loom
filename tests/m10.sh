#!/usr/bin/env bash
# Milestone 11: moving to a home laptop — doctor, backup rotation, restore
# round-trip (including starting the restored instance), the installer, the
# static binary from scripts/dist.sh (when built), and the Forgejo import
# script against a stub API.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
DATA=${LOOM_DATA:-$ROOT/data}
LOOM=${LOOM:-$ROOT/target/release/loom}
W=$(mktemp -d "${TMPDIR:-/tmp}/loom-m10.XXXXXX")
SRV=
cleanup() { [ -n "$SRV" ] && kill "$SRV" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
cd "$W" || exit 1
export GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }

echo "== doctor"
# Free ports: this machine may already have things on 3000/2222.
D=$(LOOM_DATA="$W/doctor-data" LOOM_ADDR=127.0.0.1:3931 LOOM_SSH_ADDR=127.0.0.1:2932 $LOOM doctor 2>&1); rc=$?
check "doctor exits 0 on a healthy machine"     is "$rc" 0
check "  reports git"                           has "$D" "git "
check "  reports the data directory"            has "$D" "data directory"
check "doctor notices busy ports"               bash -c "LOOM_DATA=$W/d2 LOOM_ADDR=10.255.255.1:1 LOOM_SSH_ADDR=off $LOOM doctor 2>&1 | grep -q 'IN USE'"

echo "== backup rotation"
for i in 1 2 3; do LOOM_DATA=$DATA $LOOM backup "$W/bk" --repos --keep 2 >/dev/null || exit 1; sleep 1; done
check "keeps only the newest 2 sets"            is "$(ls "$W/bk"/loom-*.db | wc -l)" 2
check "  and their repositories"                is "$(ls -d "$W/bk"/repos-* | wc -l)" 2
DB=$(ls -t "$W/bk"/loom-*.db | head -1); REPOS=$(ls -dt "$W/bk"/repos-* | head -1)
check "  every kept set has both parts"         bash -c "[ -n '$REPOS' ] && [ -d '$REPOS/alice' ]"

echo "== restore round-trip"
check "restore into a fresh directory"          is "$(LOOM_DATA="$W/restored" $LOOM restore "$DB" --repos "$REPOS" >/dev/null 2>&1; echo $?)" 0
check "  database is there"                     test -f "$W/restored/loom.db"
check "  repositories landed at repos/<owner>"  test -d "$W/restored/repos/alice/ytm-tui.git"
check "refuses to overwrite without --force"    bash -c "! LOOM_DATA='$W/restored' $LOOM restore '$DB' >/dev/null 2>&1"
check "  --force replaces it"                   is "$(LOOM_DATA="$W/restored" $LOOM restore "$DB" --repos "$REPOS" --force >/dev/null 2>&1; echo $?)" 0
LOOM_DATA="$W/restored" LOOM_ADDR=127.0.0.1:3921 LOOM_SSH_ADDR=127.0.0.1:2922 setsid $LOOM serve >"$W/restored.log" 2>&1 &
SRV=$!
for i in $(seq 40); do curl -sf http://127.0.0.1:3921/healthz >/dev/null && break; sleep 0.25; done
check "  the restored instance serves pages"    is "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3921/alice/ytm-tui)" 200
check "  accounts survived"                     is "$(curl -s -o /dev/null -w '%{http_code}' -d 'name=alice&password=alice-test-pw' http://127.0.0.1:3921/login)" 303
check "  repositories survived"                  bash -c "GIT_CONFIG_GLOBAL=/dev/null git ls-remote http://127.0.0.1:3921/alice/ytm-tui.git | grep -q refs/heads/"
kill "$SRV" 2>/dev/null; SRV=

echo "== installer (no systemd)"
mkdir -p "$W/pkg" && cp "$LOOM" "$W/pkg/loom" && cp -r "$ROOT/deploy" "$W/pkg/deploy"
HOME="$W/home" PREFIX="$W/prefix" sh "$W/pkg/deploy/install.sh" --no-service >"$W/install.log" 2>&1
check "installs the binary under PREFIX"        test -x "$W/prefix/bin/loom"
check "creates the data directory"              test -d "$W/prefix/share/loom"
check "writes an environment file"              bash -c "grep -q '^LOOM_URL=http://' $W/home/.config/loom/env"
check "installs the unit and backup timer"      bash -c "[ -f $W/home/.config/systemd/user/loom.service ] && [ -f $W/home/.config/systemd/user/loom-backup.timer ]"
check "  --no-service doesn't start anything"   bash -c "! grep -q 'starts at boot' $W/install.log"
check "  prints the first-admin step"           bash -c "grep -q 'user add' $W/install.log"

echo "== static binary"
TGZ=$(ls "$ROOT"/dist/loom-*-linux-*.tar.gz 2>/dev/null | head -1)
if [ -n "$TGZ" ]; then
    tar -C "$W" -xzf "$TGZ"
    BIN=$(ls "$W"/loom-*-linux-*/loom | head -1)
    check "the tarball holds a static binary"    bash -c "file '$BIN' | grep -qE 'static(-pie)? linked'"
    check "  it runs and reports its version"    bash -c "'$BIN' version | grep -q '^loom 0\.'"
    check "  tarball includes the installer"     bash -c "[ -f '$(dirname "$BIN")/deploy/install.sh' ] && [ -f '$(dirname "$BIN")/docs/home-laptop.md' ] || [ -f '$(dirname "$BIN")/README-HOME.md' ]"
else
    echo "  (no dist/ tarball yet: run scripts/dist.sh)"
fi

echo "== forgejo import"
login alice alice-test-pw
mkdir -p "$W/fixt"
git init -q -b main "$W/src" && cd "$W/src"
FIXT_ENV="GIT_AUTHOR_NAME=s GIT_AUTHOR_EMAIL=s@s GIT_COMMITTER_NAME=s GIT_COMMITTER_EMAIL=s@s"
echo hello > file.txt && git add . && env $FIXT_ENV git commit -qm one
env $FIXT_ENV git tag v1.0.0
cd "$W"
cat > "$W/fixt/repos.json" <<EOF
[{"name":"stubrepo","default_branch":"main","private":false,"description":"stubbed repo","clone_url":"$W/src"}]
EOF
cat > "$W/fixt/labels.json" <<'EOF'
[{"name":"imported-bug","color":"#d73a4a","description":"came from forgejo"}]
EOF
cat > "$W/fixt/milestones.json" <<'EOF'
[{"title":"v1.1","description":"done already","state":"closed"},{"title":"v1.2","description":"","state":"open"}]
EOF
cat > "$W/fixt/issues.json" <<'EOF'
[{"number":1,"title":"first stub issue","body":"body one","state":"open","labels":[{"name":"imported-bug"}],"milestone":{"title":"v1.2"},"user":{"login":"stubby"}},
 {"number":2,"title":"second, closed","body":"body two","state":"closed","labels":[],"milestone":null,"user":{"login":"stubby"}},
 {"number":3,"title":"looks like a PR","body":"","state":"open","labels":[],"milestone":null,"pull_request":{},"user":{"login":"stubby"}}]
EOF
echo '[{"body":"a migrated comment"}]' > "$W/fixt/comments-2.json"
cat > "$W/fixt/releases.json" <<'EOF'
[{"tag_name":"v1.0.0","name":"One Point Oh","body":"release notes","draft":false,"prerelease":true},
 {"tag_name":"v9.9.9","name":"Ghost","body":"","draft":false,"prerelease":false}]
EOF
python3 -I "$ROOT/tests/forgejo-stub.py" 3933 "$W/fixt" &
SRV=$!
for i in $(seq 40); do curl -sf http://127.0.0.1:3933/api/v1/users/stubby/repos >/dev/null && break; sleep 0.25; done
SSH_CMD="ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT"
check "import repos + issues + releases"          bash -c "LOOM_DATA='$DATA' LOOM_URL='$B' PUSH_BASE='ssh://git@127.0.0.1:$SSH_PORT' GIT_SSH_COMMAND='$SSH_CMD' '$ROOT/scripts/import-forgejo.sh' --from http://127.0.0.1:3933 --owner stubby --loom-owner alice --loom-bin '$LOOM' --issues --releases"
check "repository landed"                        is "$(curl -s -o /dev/null -w '%{http_code}' $B/alice/stubrepo)" 200
check "default branch came along"                is "$(git --git-dir="$DATA/repos/alice/stubrepo.git" symbolic-ref --short HEAD)" main
check "label imported"                           has "$(curl -s $B/alice/stubrepo/labels)" "imported-bug"
check "issues migrated, PR skipped"              bash -c "o=\$(curl -s $B/api/v1/repos/alice/stubrepo/issues | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)))'); c=\$(curl -s '$B/api/v1/repos/alice/stubrepo/issues?state=closed' | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)))'); [ \$((o + c)) = 2 ]"
check "  comment migrated"                       has "$(curl -s $B/alice/stubrepo/issues/2)" "a migrated comment"
check "  closed state kept"                      has "$(curl -s $B/alice/stubrepo/issues/2)" "✓ closed"
check "  label + milestone attached"             bash -c "p=\$(curl -s $B/alice/stubrepo/issues/1); grep -q imported-bug <<<\"\$p\" && grep -q 'v1.2' <<<\"\$p\""
check "  closed milestone kept closed"          has "$(curl -s $B/alice/stubrepo/milestones)" "v1.1"
check "release notes migrated"                   has "$(curl -s $B/alice/stubrepo/releases)" "One Point Oh"
check "  pre-release flag migrated"              has "$(curl -s $B/alice/stubrepo/releases)" "pre-release"
check "release without that tag skipped"        lacks "$(curl -s $B/alice/stubrepo/releases)" "Ghost"
check "a second run skips the existing repo"    bash -c "LOOM_DATA='$DATA' LOOM_URL='$B' PUSH_BASE='ssh://git@127.0.0.1:$SSH_PORT' GIT_SSH_COMMAND='$SSH_CMD' '$ROOT/scripts/import-forgejo.sh' --from http://127.0.0.1:3933 --owner stubby --loom-owner alice --loom-bin '$LOOM' --issues --releases 2>&1 | grep -q 'skip'"
check "  and didn't duplicate the issues"       bash -c "o=\$(curl -s $B/api/v1/repos/alice/stubrepo/issues | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)))'); c=\$(curl -s '$B/api/v1/repos/alice/stubrepo/issues?state=closed' | python3 -I -c 'import json,sys; print(len(json.load(sys.stdin)))'); [ \$((o + c)) = 2 ]"
kill "$SRV" 2>/dev/null; SRV=
post alice /alice/stubrepo/settings/delete -d confirm=alice/stubrepo >/dev/null

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
