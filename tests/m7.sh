#!/usr/bin/env bash
# Milestone 8: syntax highlighting, blob/diff caps, blame, releases + assets +
# source archives, TOTP two-factor, per-IP login throttling, pull mirrors,
# CLI 2FA recovery, git process cap. Fresh repos per run.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
DATA=${LOOM_DATA:-$ROOT/data}
LOOM=${LOOM:-$ROOT/target/release/loom}
RUN=$$
REPO=m7r$RUN
SRC=m7s$RUN
MIR=m7m$RUN
R=alice/$REPO
W=$(mktemp -d "${TMPDIR:-/tmp}/loom-m7.XXXXXX")
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
cleanup() {
  for r in $R alice/$SRC alice/$MIR; do post alice /$r/settings/delete -d confirm=$r >/dev/null 2>&1; done
  LOOM_DATA=$DATA $LOOM user 2fa-off bob >/dev/null 2>&1
  rm -rf "$W"
}
trap cleanup EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
export GIT_SSH_COMMAND="ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT"
S="ssh://git@127.0.0.1:$SSH_PORT"

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
# login_status <jar> <user> <pass> -> http code, keeps cookies
login_status() { curl -s -o /dev/null -D "$W/login.h" -w '%{http_code}' -c "$W/$1.jar" -d "name=$2&password=$3" "$B/login"; }
page() { curl -s -b "$W/$1.jar" "$B$2"; }
get_code() { curl -s -o /dev/null -w '%{http_code}' -b "$W/$1.jar" "$B$2"; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
totp() { python3 -I - "$1" "${2:-0}" <<'PY'
import base64, hashlib, hmac, struct, sys, time
s = sys.argv[1].upper().strip()
key = base64.b32decode(s + "=" * (-len(s) % 8))
c = int(time.time()) // 30 + int(sys.argv[2])
d = hmac.new(key, struct.pack(">Q", c), hashlib.sha1).digest()
print("%06d" % ((struct.unpack(">I", d[d[-1] & 15:][:4])[0] & 0x7FFFFFFF) % 1000000))
PY
}

login alice alice-test-pw; : > "$W/bob.jar"

echo "== setup"
check "create $R"                                is "$(post alice /new -d owner=alice -d name=$REPO -d description=features)" 303
git init -q -b main w && cd w
printf 'fn main() {\n    // say <hello>\n    let s = "x & y";\n    println!("{}", 42);\n}\n' > src.rs
printf 'plain <b> text\n' > notes.unknown
for i in $(seq 1 6000); do echo "line $i"; done > big.txt
echo readme > README.md
git add . && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=fel@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=fel@x git commit -qm "initial"
git tag v1 && git push -q $S/$R.git main v1 && cd ..

echo "== highlighting (issue #4)"
P=$(page alice /$R/blob/main/src.rs)
check "keywords highlighted"                     has "$P" '<span class="tok-k">fn</span>'
check "strings highlighted (quotes escaped)"     has "$P" '<span class="tok-s">&quot;x &amp; y&quot;</span>'
check "comment highlighted, still escaped"       has "$P" '&lt;hello&gt;'
check "raw HTML in source stays escaped"         lacks "$P" "<hello>"
check "unknown extension: no tokens, escaped"    bash -c "p=\$(curl -s $B/$R/blob/main/notes.unknown); grep -q '&lt;b&gt;' <<<'$P' || grep -q '&lt;b&gt;' <<<\"\$p\"; ! grep -q 'tok-' <<<\"\$p\""

echo "== page caps (issue #5)"
P=$(page alice /$R/blob/main/big.txt)
check "blob view capped at 5000 lines"           has "$P" "Showing the first 5000 of 6000 lines"
check "raw file still complete"                  is "$(curl -s $B/$R/raw/main/big.txt | wc -l)" 6000

echo "== blame"
P=$(page alice /$R/blame/main/src.rs)
check "blame shows the commit"                   bash -c "curl -s $B/$R/blame/main/src.rs | grep -qE '/commit/[0-9a-f]{40}'"
check "blame shows line text"                    has "$P" "println!"
printf 'fn main() {\n    // say <hello>\n    let s = "x & y";\n    println!("{}", 42);\n    // new line\n}\n' > w/src.rs
cd w && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=fel@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=fel@x git commit -qam "add a line" && git push -q $S/$R.git main && cd ..
check "blame has two commits after an edit"      is "$(page alice /$R/blame/main/src.rs | grep -oE '/commit/[0-9a-f]{40}' | sort -u | wc -l)" 2
check "blame of a missing file -> 404"           is "$(get_code alice /$R/blame/main/nope.rs)" 404

echo "== releases"
check "release with a missing tag refused"       is "$(post alice /$R/releases/new -d tag=v9 -d title=x -d body=)" 400
check "create release on v1"                     is "$(post alice /$R/releases/new -d tag=v1 -d title=First -d body=notes)" 303
RL=$(page alice /$R/releases | grep -oE '/releases/[0-9]+' | head -1 | grep -oE '[0-9]+')
check "release page shows tag and notes"         bash -c "p=\$(curl -s $B/$R/releases/$RL); grep -q 'v1' <<<'$P' || grep -q 'v1' <<<\"\$p\"; grep -q 'notes' <<<\"\$p\""
printf 'binary\x00payload\n' > asset.bin
check "upload an asset"                          is "$(post alice /$R/releases/$RL/assets -F file=@asset.bin)" 303
AID=$(page alice /$R/releases/$RL | grep -oE '/releases/assets/[0-9]+' | head -1 | grep -oE '[0-9]+')
check "download returns the bytes"               bash -c "curl -s $B/$R/releases/assets/$AID | cmp -s - asset.bin"
check "download counted"                         has "$(page alice /$R/releases/$RL)" "1 download"
check "download has attachment header"           bash -c "curl -sI $B/$R/releases/assets/$AID | grep -qi 'content-disposition: attachment'"
check "source archive is a real tarball"         bash -c "curl -s $B/$R/archive/v1.tar.gz | tar tzf - | grep -q 'src.rs'"
login_status bob bob bob-test-pw1 >/dev/null
check "signed-in bob (no access) -> 404"         is "$(post bob /$R/releases/$RL/assets -F file=@asset.bin)" 404
check "delete asset"                             is "$(post alice /$R/releases/$RL/assets/$AID/delete)" 303

echo "== two-factor (TOTP)"
SEC=$(page bob /settings/2fa | grep -oE '<pre>[A-Z2-7]{16,}</pre>' | sed 's/<[^>]*>//g')
check "setup page offers a secret"               test "${#SEC}" -ge 16
check "wrong code doesn't enable"                is "$(post bob /settings/2fa/enable -d secret=$SEC -d code=000000)" 400
check "right code enables"                       is "$(post bob /settings/2fa/enable -d secret=$SEC -d code=$(totp $SEC))" 200
check "login now needs a code"                   is "$(login_status bob2 bob bob-test-pw1)" 303
check "  bounced to the 2FA page"                has "$(cat $W/login.h)" "location: /login/totp"
check "  pending session can't open settings"    is "$(get_code bob2 /settings/keys)" 303
check "  wrong code rejected"                    is "$(post bob2 /login/totp -d code=123456)" 401
check "  right code completes login"             is "$(post bob2 /login/totp -d code=$(totp $SEC))" 303
check "  signed in afterwards"                   is "$(get_code bob2 /settings/keys)" 200
check "  previous session unaffected? (no)"      bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/bob.jar $B/settings/keys) = 200 ]"
check "cli 2fa-off recovers the account"         bash -c "LOOM_DATA=$DATA $LOOM user 2fa-off bob && [ \$(curl -s -o /dev/null -w '%{http_code}' -c $W/bob3.jar -d 'name=bob&password=bob-test-pw1' $B/login) = 303 ]"
check "  login without a code works again"       is "$(get_code bob3 /settings/keys)" 200

echo "== per-IP throttle without cross-user lockout (#1)"
# Burn attempts against a throwaway account so this suite stays repeatable
# (the throttle is in-memory, so a restart also clears it).
LOOM_DATA=$DATA $LOOM user add throttle$RUN --password-stdin <<<"throwaway-pw" >/dev/null 2>&1
for i in 1 2 3 4 5; do login_status evil throttle$RUN wrongpass$i >/dev/null; done
check "that user+IP pair is now throttled"       has "$(cat $W/login.h)" "HTTP/1.1 401"
check "  other users are unaffected"             is "$(login_status good bob bob-test-pw1)" 303
check "  ...and are signed in"                   is "$(get_code good /settings/keys)" 200
check "  alice is unaffected"                   is "$(login_status m alice alice-test-pw)" 303
check "cleanup the throwaway user"               bash -c "LOOM_DATA=$DATA $LOOM user del throttle$RUN"

echo "== pull mirror"
check "create source repo"                       is "$(post alice /new -d owner=alice -d name=$SRC -d description=src)" 303
git clone -q $S/alice/$SRC.git src && cd src && echo hi > hi.txt && git add . && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=fel@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=fel@x git commit -qm src && git push -q origin main && cd ..
check "create mirror target repo"                is "$(post alice /new -d owner=alice -d name=$MIR -d description=mirror)" 303
check "mirror URL must be a remote"              is "$(post alice /alice/$MIR/settings/mirror -d 'url=/etc/passwd' -d hours=6)" 400
check "set up the mirror (first sync now)"       is "$(post alice /alice/$MIR/settings/mirror --data-urlencode "url=$B/alice/$SRC.git" -d hours=6)" 200
check "  target got the source's main"           is "$(git --git-dir=$DATA/repos/alice/$MIR.git rev-parse main)" "$(git --git-dir=$DATA/repos/alice/$SRC.git rev-parse main)"
check "  pushes to a mirror are refused"         bash -c "! (cd src && GIT_SSH_COMMAND='$GIT_SSH_COMMAND' git push -q $S/alice/$MIR.git main 2>&1)"
check "manual sync says finished"                is "$(post alice /alice/$MIR/settings/mirror/sync)" 200
check "removing the mirror re-allows pushes"     bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/alice.jar -H 'Origin: $B' -X POST $B/alice/$MIR/settings/mirror/delete) = 200 ] && (cd src && GIT_SSH_COMMAND='$GIT_SSH_COMMAND' git push -q $S/alice/$MIR.git main)"

echo "== git process cap (#2)"
check "12 concurrent clones still succeed"       bash -c "pids=(); for i in \$(seq 12); do (cd $W && GIT_CONFIG_GLOBAL=/dev/null git -c credential.helper= clone -q --bare $B/alice/$SRC.git c\$i) & pids+=(\$!); done; ok=0; for p in \${pids[@]}; do wait \$p && ok=\$((ok+1)); done; [ \$ok = 12 ]"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
