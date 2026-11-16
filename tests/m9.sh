#!/usr/bin/env bash
# Milestone 10: templates, draft/prerelease releases, avatars, graph view,
# opt-in last-commit badges, org teams, email notifications (MTA command),
# LFS garbage collection, rate limiting. Fresh repo/org per run.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
LOOM=${LOOM:-$HOME/Projects/loom/target/release/loom}
GITLFS=${GITLFS:-$HOME/.cache/loom-bench/bin}
RUN=$$
REPO=t9r$RUN
R=alice/$REPO
ORG=acme$RUN
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m9.XXXXXX")
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
getc() { curl -s -o "$W/last.html" -w '%{http_code}' -b "$W/$1.jar" "$B$2"; }
page() { curl -s -b "$W/$1.jar" "$B$2"; }
cleanup() {
  post alice /$R/settings/delete -d confirm=$R >/dev/null 2>&1
  post alice /$ORG/tpriv/settings/delete -d confirm=$ORG/tpriv >/dev/null 2>&1
  post alice /$ORG/settings/delete -d confirm=$ORG >/dev/null 2>&1
  sqlite3 "$DATA/loom.db" "delete from mail_outbox where to_addr like '%@example.com'" 2>/dev/null
  rm -rf "$W"
}
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
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
mention() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode "title=$3" --data-urlencode "body=$4" "$B/$2/issues/new" | grep -oE '[0-9]+$'; }

login alice alice-test-pw; login bob bob-test-pw1; : > "$W/anon.jar"
echo "== setup"
check "create $R"                               is "$(post alice /new -d owner=alice -d name=$REPO -d description=m9)" 303
git init -q -b main w && cd w
mkdir -p .loom/ISSUE_TEMPLATE .github
printf '### What happened?\n\n### Steps\n\n1. \n\n<!-- template-body -->\n' > .loom/ISSUE_TEMPLATE/bug.md
printf '## Summary\n\n<!-- pr-template -->\n' > .github/pull_request_template.md
printf 'fn main() { println!("hi"); }\n' > src.rs
echo readme > README.md
git add -A && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=f@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=f@x git commit -qm init
git tag v1 && git push -q $S/$R.git main v1 && cd ..

echo "== templates"
check "new issue lists the template"            has "$(page alice /$R/issues/new)" "bug.md"
check "?template= prefills the body"            has "$(page alice "/$R/issues/new?template=bug.md")" "What happened?"
(cd w && git checkout -qb feat && echo f > f.txt && git add f.txt && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=f@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=f@x git commit -qm feat && git push -q $S/$R.git feat)
check "PR compare prefills its template"        has "$(page alice "/$R/compare?base=main&head=feat")" "pr-template"

echo "== draft / prerelease releases"
check "create draft release"                    is "$(post alice /$R/releases/new -d tag=v1 -d title=Drafty -d body=draft -d draft=1)" 303
RL=$(page alice /$R/releases | grep -oE '/releases/[0-9]+' | head -1 | grep -oE '[0-9]+')
check "  author sees the draft badge"           has "$(page alice /$R/releases)" "draft"
check "  anonymous doesn't see it"              lacks "$(page anon /$R/releases)" "Drafty"
check "  anonymous can't open it"               is "$(getc anon /$R/releases/$RL)" 404
check "publish it (edit, no draft)"             is "$(post alice /$R/releases/$RL/edit -d title=Drafty -d body=live)" 303
check "  now public"                            has "$(page anon /$R/releases)" "Drafty"
check "mark prerelease"                         is "$(post alice /$R/releases/$RL/edit -d title=Drafty -d body=live -d prerelease=1)" 303
check "  badge shown"                           has "$(page anon /$R/releases)" "pre-release"

echo "== avatars, graph, last-commit (opt-in)"
check "avatar is an SVG"                        bash -c "curl -s $B/avatar/alice | grep -q '<svg'"
check "  cached (304)"                          bash -c "e=\$(curl -sI $B/avatar/alice | grep -i etag | cut -d' ' -f2 | tr -d '\r'); [ \$(curl -s -o /dev/null -w '%{http_code}' -H \"If-None-Match: \$e\" $B/avatar/alice) = 304 ]"
check "graph view draws lanes"                  has "$(page anon /$R/graph/main)" "* "
check "tree has no badges by default"           lacks "$(page anon /$R/tree/main)" "last-commit"
check "enable last-commit badges"               is "$(post alice /$R/settings/general -d description=m9 -d show_last_commit=1)" 200
check "  badges appear"                         has "$(page anon /$R/tree/main)" "last-commit"
check "disable them again"                      is "$(post alice /$R/settings/general -d description=m9)" 200

echo "== org teams"
check "create org"                              is "$(post alice /new/org -d name=$ORG -d description=teams)" 303
check "org has a private repo"                  is "$(post alice /new -d owner=$ORG -d name=tpriv -d description= -d private=1)" 303
check "bob can't see it yet"                    is "$(getc bob /$ORG/tpriv)" 404
check "create team core"                        is "$(post alice /$ORG/settings/teams -d name=core -d description=core-team)" 200
TID=$(page alice /$ORG/settings | grep -oE "/settings/teams/[0-9]+" | head -1 | grep -oE '[0-9]+')
check "add bob to the team (read)"              is "$(post alice /$ORG/settings/teams/$TID/members -d name=bob -d role=read)" 200
check "grant the private repo (read)"           is "$(post alice /$ORG/settings/teams/$TID/repos -d repo=$ORG/tpriv -d role=read)" 200
check "  bob can now see it"                    is "$(getc bob /$ORG/tpriv)" 200
check "  bob still can't change settings"         bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/bob.jar -H 'Origin: $B' -X POST -d description=x $B/$ORG/tpriv/settings/general) != 200 ]"
check "bob can't manage teams"                  is "$(post bob /$ORG/settings/teams -d name=x -d description=)" 404
check "revoke the grant"                        is "$(post alice /$ORG/settings/teams/$TID/repos/remove -d repo=$ORG/tpriv)" 200
check "  bob loses access"                      is "$(getc bob /$ORG/tpriv)" 404
check "repo settings list the team grant"       bash -c "curl -s -o /dev/null -b $W/alice.jar -H 'Origin: $B' -d repo=$ORG/tpriv -d role=write $B/$ORG/settings/teams/$TID/repos; curl -s -b $W/alice.jar $B/$ORG/tpriv/settings | grep -q '>core<'"

echo "== email notifications"
check "bob opts in with an email"               is "$(post bob /settings/account -d email=bob@example.com -d notify=1)" 200
check "bob rejects a bad email"                 is "$(post bob /settings/account -d 'email=not an email' -d notify=1)" 400
MI=$(mention alice $R "email test $RUN" "hello @bob, look")
check "mention queues an email"                 bash -c "[ \$(sqlite3 -readonly '$DATA/loom.db' \"select count(*) from mail_outbox where to_addr='bob@example.com' and subject like '%email test $RUN%'\") -ge 1 ]"
printf '#!/bin/sh\ncat >> %s/mail.log\n' "$W" > mailer.sh && chmod +x mailer.sh
check "loom mail flush sends it"                bash -c "LOOM_DATA=$DATA LOOM_MAIL_CMD='$W/mailer.sh -t' $LOOM mail flush | grep -qE '[1-9][0-9]* sent'"
check "  message looks like RFC 822"            bash -c "grep -q 'To: bob@example.com' $W/mail.log && grep -q 'Subject: \[' $W/mail.log && grep -q '$R/issues/' $W/mail.log"
check "  nothing left queued for bob"           bash -c "[ \$(sqlite3 -readonly '$DATA/loom.db' \"select count(*) from mail_outbox where to_addr='bob@example.com' and sent=0\") = 0 ]"

echo "== LFS garbage collection"
# oid is the sha256 of the exact bytes we upload.
printf 'kept %s\n' $RUN > kept.payload; OID1=$(sha256sum kept.payload | cut -d' ' -f1)
printf 'orphan %s\n' $RUN > orphan.payload; OID2=$(sha256sum orphan.payload | cut -d' ' -f1)
for pair in "kept.payload $OID1" "orphan.payload $OID2"; do
  set -- $pair; f=$1; oid=$2
  UP=$(curl -s -u alice:alice-test-pw -H 'Accept: application/vnd.git-lfs+json' -H 'Content-Type: application/vnd.git-lfs+json' -d "{\"operation\":\"upload\",\"objects\":[{\"oid\":\"$oid\",\"size\":$(stat -c %s $f)}]}" $B/$R.git/info/lfs/objects/batch)
  HREF=$(python3 -I -c 'import json,sys; print(json.load(sys.stdin)["objects"][0]["actions"]["upload"]["href"])' <<<"$UP")
  echo "  upload $f -> $(curl -s -o /dev/null -w '%{http_code}' -u alice:alice-test-pw -X PUT --data-binary @$f "$HREF")"
done
cd w && git checkout -q main && git pull -q "$S/$R.git" main && cd ..
cd w
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 80\n' "$OID1" > kept.bin
git add kept.bin && GIT_AUTHOR_NAME=fel GIT_AUTHOR_EMAIL=f@x GIT_COMMITTER_NAME=fel GIT_COMMITTER_EMAIL=f@x git commit -qm "pointer for kept" && git push -q $S/$R.git main
cd ..
check "gc --dry-run reports the orphan"         bash -c "LOOM_DATA=$DATA $LOOM lfs gc --dry-run --repo $R | grep -q 'would remove .*$OID2'"
check "  and not the referenced object"         bash -c "! LOOM_DATA=$DATA $LOOM lfs gc --dry-run --repo $R | grep -q '$OID1'"
check "gc removes it"                           bash -c "LOOM_DATA=$DATA $LOOM lfs gc --repo $R | grep -q 'removed 1 objects'"
check "  orphan is gone, referenced still served" bash -c "u() { curl -s -o /dev/null -w '%{http_code}' -u alice:alice-test-pw $B/$R.git/info/lfs/objects/\$1; }; [ \$(u $OID1) = 200 ] && [ \$(u $OID2) = 404 ]"

echo "== rate limiting (writes, default 100/s burst 200)"
: > "$W/codes"
for i in $(seq 260); do curl -s -o /dev/null -w '%{http_code}\n' -X POST -b "$W/anon.jar" -H "Origin: $B" -d x=1 "$B/nonexistent-$i" >> "$W/codes" & done
wait
codes=$(cat "$W/codes")
check "a write burst is partly refused (429)"   bash -c "grep -q 429 <<<'$codes'"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
