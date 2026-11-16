#!/usr/bin/env bash
# Milestone 3: stars, forks, collaborators, branch protection, archive,
# rename/transfer/delete, orgs. Run against a server seeded like smoke.sh.
# Git runs with an empty config: no credential helpers, never any prompt/GUI.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
LOOM=${LOOM:-$HOME/Projects/loom/target/release/loom}
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m3.XXXXXX")
trap 'rm -rf "$W"' EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
g() { git -c credential.helper= "$@"; }
as() { local who=$1; shift; GIT_SSH_COMMAND="ssh -i $KEYS/$who -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT" "$@"; }
S="ssh://git@127.0.0.1:$SSH_PORT"

pass=0 fail=0
check() { local name=$1; shift; printf '%-58s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
refuse() { local name=$1; shift; printf '%-58s' "$name"; if out=$("$@" 2>&1); then echo "FAIL: allowed"; fail=$((fail+1)); else echo "ok (refused)"; pass=$((pass+1)); fi; }

login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
# post <user> <path> <form...>  -> prints HTTP status
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
get() { curl -s -o "$W/last.html" -w '%{http_code}' -b "$W/$1.jar" "$B$2"; }
is() { [ "$1" = "$2" ] || { echo "got $1, want $2"; return 1; }; }
# Subshell checks need these too (a missing function would fake a "refused").
export -f g as post get is 2>/dev/null
export W B KEYS SSH_PORT S DATA
page_has() { grep -q -- "$1" "$W/last.html" || { echo "page lacks: $1"; return 1; }; }

login alice alice-test-pw; login bob bob-test-pw1

echo "== stars"
check "bob stars alice/ytm-tui"            is "$(post bob /alice/ytm-tui/star)" 303
check "  count shows 1, button says starred" bash -c "curl -s -b $W/bob.jar $B/alice/ytm-tui | grep -q '★ starred<span class=\"count\">1'"
check "bob unstars"                          is "$(post bob /alice/ytm-tui/star)" 303
check "  count back to 0"                    bash -c "curl -s $B/alice/ytm-tui | grep -q '<span class=\"count\">0</span>'"
check "anon star -> login redirect"          is "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Origin: $B" $B/alice/ytm-tui/star)" 303

echo "== forks"
check "bob forks alice/ytm-tui"             is "$(post bob /alice/ytm-tui/fork -d owner=bob -d name=ytm-fork)" 303
check "  bob/ytm-fork shows 'forked from'"   bash -c "curl -s $B/bob/ytm-fork | grep -q 'forked from <a href=\"/alice/ytm-tui\"'"
check "  parent fork count is 1"             bash -c "curl -s $B/alice/ytm-tui | grep -q '⑂ fork<span class=\"count\">1\|⑂ <span class=\"count\">1'"
check "  fork is clonable, same HEAD"        bash -c "g clone -q $B/bob/ytm-fork.git f1 && [ \$(git -C f1 rev-parse HEAD) = \$(git --git-dir=$DATA/repos/alice/ytm-tui.git rev-parse HEAD) ]"
check "  fork has no 'origin' remote"        bash -c "! git --git-dir=$DATA/repos/bob/ytm-fork.git remote | grep -q ."
refuse "bob forks alice/secret (no access)" bash -c "[ \$(post bob /alice/secret/fork -d owner=bob -d name=s) = 303 ]"
refuse "bob forks into alice's namespace"   bash -c "[ \$(post bob /alice/slate/fork -d owner=alice -d name=x) = 303 ]"

echo "== collaborators"
refuse "bob can't open alice/secret settings" bash -c "[ \$(get bob /alice/secret/settings) = 200 ]"
check "alice adds bob as write"             is "$(post alice /alice/secret/settings/collaborators -d name=bob -d role=write)" 200
check "  bob ssh-clones secret"              as bob g clone -q $S/alice/secret.git c1
git -C c1 commit -q --allow-empty -m "bob was here"
check "  bob ssh-pushes secret"              as bob g -C c1 push -q origin HEAD:main
check "  bob sees settings? (no: write<admin)" bash -c "[ \$(get bob /alice/secret/settings) = 404 ]"
check "alice downgrades bob to read"        is "$(post alice /alice/secret/settings/collaborators -d name=bob -d role=read)" 200
git -C c1 commit -q --allow-empty -m "should bounce"
refuse "  bob push refused with read"        as bob g -C c1 push -q origin HEAD:main
check "alice removes bob"                   is "$(post alice /alice/secret/settings/collaborators/remove -d name=bob)" 200
refuse "  bob clone refused after removal"   as bob g clone -q $S/alice/secret.git c2

echo "== branch protection"
check "alice protects main"                 is "$(post alice /alice/secret/settings/protect -d branch=main)" 200
as alice g clone -q $S/alice/secret.git p1
git -C p1 commit -q --allow-empty -m "ff commit"
check "  fast-forward push allowed"          as alice g -C p1 push -q origin HEAD:main
git -C p1 reset -q --hard HEAD~1; git -C p1 commit -q --allow-empty -m "rewritten"
refuse "  force-push refused (ssh)"          as alice g -C p1 push -q -f origin HEAD:main
check "  ...with a clear message"            bash -c "as() { GIT_SSH_COMMAND=\"ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT\" \"\$@\"; }; as git -c credential.helper= -C p1 push -f origin HEAD:main 2>&1 | grep -q \"protected: force-push not allowed\""
refuse "  force-push refused (http)"         g -C p1 push -q -f http://alice:alice-test-pw@${B#http://}/alice/secret.git HEAD:main
refuse "  deleting main refused"             as alice g -C p1 push -q origin :main
check "  other branches unaffected"          bash -c "as() { GIT_SSH_COMMAND=\"ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT\" \"\$@\"; }; as git -C p1 push -q origin HEAD:refs/heads/scratch && as git -C p1 push -q -f origin HEAD~1:refs/heads/scratch && as git -C p1 push -q origin :scratch"
check "alice unprotects main"               is "$(post alice /alice/secret/settings/protect/remove -d branch=main)" 200
check "  force-push now allowed"             as alice g -C p1 push -q -f origin HEAD:main

echo "== archive"
check "archive secret"                       is "$(post alice /alice/secret/settings/general -d description=x -d archived=1 -d private=1)" 200
check "  archived badge shown"               bash -c "curl -s -b $W/alice.jar $B/alice/secret | grep -q 'archived</span>'"
git -C p1 commit -q --allow-empty -m "into archive"
refuse "  owner push refused while archived" as alice g -C p1 push -q origin HEAD:main
check "  clone still works"                  as alice g clone -q $S/alice/secret.git a1
check "unarchive"                            is "$(post alice /alice/secret/settings/general -d 'description=private test repo' -d private=1)" 200
check "  push works again"                   as alice g -C p1 push -q origin HEAD:main

echo "== rename"
check "rename secret -> secret2"             is "$(post alice /alice/secret/settings/rename -d name=secret2)" 303
check "  old URL 404"                        is "$(get alice /alice/secret)" 404
check "  new URL 200"                        is "$(get alice /alice/secret2)" 200
check "  dir moved on disk"                  test -d $DATA/repos/alice/secret2.git -a ! -e $DATA/repos/alice/secret.git
check "  clone via new name"                 as alice g clone -q $S/alice/secret2.git r1
check "rename back"                          is "$(post alice /alice/secret2/settings/rename -d name=secret)" 303
refuse "rename onto existing repo"           bash -c "[ \$(post alice /alice/secret/settings/rename -d name=slate) = 303 ]"

echo "== orgs"
check "alice creates org acme"              is "$(post alice /new/org -d name=acme -d 'description=test org')" 303
refuse "org name clash with user"            bash -c "[ \$(post alice /new/org -d name=bob -d description=) = 303 ]"
check "transfer alice/slate -> acme"        is "$(post alice /alice/slate/settings/transfer -d owner=acme)" 303
check "  acme/slate exists, old gone"        bash -c "[ \$(get alice /acme/slate) = 200 ] && [ \$(get alice /alice/slate) = 404 ]"
git -C "$W" init -q o1; git -C o1 commit -q --allow-empty -m org
refuse "  bob (non-member) push refused"     as bob g -C o1 push -q $S/acme/slate.git HEAD:refs/heads/bob-$$
refuse "  bob can't see org settings"        bash -c "[ \$(get bob /acme/settings) = 200 ]"
check "alice adds bob to acme as write"     is "$(post alice /acme/settings/members -d name=bob -d role=write)" 200
check "  bob push to acme/slate ok"          as bob g -C o1 push -q $S/acme/slate.git HEAD:refs/heads/bob-$$
check "  bob creates acme/tools via /new"    is "$(post bob /new -d owner=acme -d name=tools -d description=)" 303
check "  org page lists members"             bash -c "curl -s $B/acme | grep -q '>bob</a>'"
check "  bob's page lists acme"              bash -c "curl -s $B/bob | grep -q 'href=\"/acme\"'"
refuse "last admin can't demote self"        bash -c "[ \$(post alice /acme/settings/members -d name=alice -d role=write) = 200 ]"
refuse "org delete blocked while it has repos" bash -c "post alice /acme/settings/delete -d confirm=acme >/dev/null
as alice g -C o1 push -q $S/alice/slate.git :refs/heads/bob-$$ 2>/dev/null; [ \$(get alice /acme) = 404 ]"

echo "== delete"
check "create alice/tmp"                    is "$(post alice /new -d owner=alice -d name=tmp -d description=)" 303
check "  wrong confirmation refused"         is "$(post alice /alice/tmp/settings/delete -d confirm=tmp)" 400
check "  correct confirmation deletes"       is "$(post alice /alice/tmp/settings/delete -d confirm=alice/tmp)" 303
check "  gone from web"                      is "$(get alice /alice/tmp)" 404
check "  kept in trash"                      bash -c "ls -d $DATA/trash/alice-tmp-*.git >/dev/null"
check "cross-site delete refused"            is "$(curl -s -o /dev/null -w '%{http_code}' -b $W/alice.jar -H 'Origin: https://evil.example' -d confirm=alice/ytm-tui $B/alice/ytm-tui/settings/delete)" 403

echo "== cleanup (restore seed state)"
post alice /acme/tools/settings/delete -d confirm=acme/tools >/dev/null
post alice /acme/slate/settings/transfer -d owner=alice >/dev/null
post bob /bob/ytm-fork/settings/delete -d confirm=bob/ytm-fork >/dev/null
post alice /acme/settings/delete -d confirm=acme >/dev/null
as alice g -C o1 push -q $S/alice/slate.git :refs/heads/bob-$$ 2>/dev/null
check "seed state restored"                  bash -c "[ \$(get alice /alice/slate) = 200 ] && [ \$(get alice /acme) = 404 ] && [ \$(get alice /bob/ytm-fork) = 404 ]"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
