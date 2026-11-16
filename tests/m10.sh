#!/usr/bin/env bash
# Milestone 11: moving to a home laptop — doctor, backup rotation, restore
# round-trip (including starting the restored instance), the installer, and
# the static binary from scripts/dist.sh (when built).
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
LOOM=${LOOM:-$HOME/Projects/loom/target/release/loom}
ROOT=${ROOT:-$HOME/Projects/loom}
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m10.XXXXXX")
SRV=
cleanup() { [ -n "$SRV" ] && kill "$SRV" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
cd "$W" || exit 1
export GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }

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
    check "  it runs and reports its version"    bash -c "'$BIN' version | grep -q '^loom 0.1'"
    check "  tarball includes the installer"     bash -c "[ -f '$(dirname "$BIN")/deploy/install.sh' ] && [ -f '$(dirname "$BIN")/docs/home-laptop.md' ] || [ -f '$(dirname "$BIN")/README-HOME.md' ]"
else
    echo "  (no dist/ tarball yet: run scripts/dist.sh)"
fi

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
