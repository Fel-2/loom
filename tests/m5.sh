#!/usr/bin/env bash
# Milestone 5: pull requests — same-repo and fork PRs, revisions, reviews,
# inline comments, merge/squash/rebase, conflicts, merged-by-push,
# require-PR protection, branch deletion. Uses a fresh repo per run.
# Git runs with an empty config: no credential helpers, never any prompt/GUI.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
ROOT=${ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
DATA=${LOOM_DATA:-$ROOT/data}
W=$(mktemp -d "${TMPDIR:-/tmp}/loom-m5.XXXXXX")
RUN=$$
REPO=pr$RUN
R=alice/$REPO
F=bob/$REPO-fork
cleanup() {
  post alice /$R/settings/delete -d confirm=$R >/dev/null 2>&1
  post bob /$F/settings/delete -d confirm=$F >/dev/null 2>&1
  rm -rf "$W"
}
trap cleanup EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
S="ssh://git@127.0.0.1:$SSH_PORT"
as() { local who=$1; shift; GIT_SSH_COMMAND="ssh -i $KEYS/$who -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT" \
  GIT_AUTHOR_NAME=$who GIT_AUTHOR_EMAIL=$who@test GIT_COMMITTER_NAME=$who GIT_COMMITTER_EMAIL=$who@test "$@"; }
bare() { git --git-dir="$DATA/repos/$1.git" "${@:2}"; }

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
page() { curl -s -b "$W/$1.jar" "$B$2"; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
count() { grep -oF -- "$2" <<<"$1" | wc -l; }
# new_pr <user> <head-spec> <title> <body> -> number
new_pr() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode base=main --data-urlencode "head=$2" --data-urlencode "title=$3" --data-urlencode "body=$4" "$B/$R/pulls/new" | grep -oE '[0-9]+$'; }
new_issue() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode "title=$2" --data-urlencode body= "$B/$R/issues/new" | grep -oE '[0-9]+$'; }
head_of() { page alice /$R/pulls/$1 | grep -oE 'name="expected_head" value="[0-9a-f]{40}"' | grep -oE '[0-9a-f]{40}'; }
merge() { post alice /$R/pulls/$1/merge -d strategy=$2 -d expected_head=$(head_of $1); }

login alice alice-test-pw; login bob bob-test-pw1

echo "== setup"
check "create $R"                               is "$(post alice /new -d owner=alice -d name=$REPO -d description=prs)" 303
as alice git init -q -b main w && cd w
printf 'line1\nline2\nline3\n' > a.txt; echo readme > README; git add .; as alice git commit -qm "initial"
check "push initial main"                       as alice git push -q $S/$R.git main
cd ..

echo "== same-repo PR, merge commit"
I=$(new_issue alice "bug to fix $RUN")
cd w; git checkout -qb feat1; echo new > b.txt; git add b.txt; as alice git commit -qm "Add b"; as alice git push -q $S/$R.git feat1; cd ..
check "compare page lists the commit"           has "$(page alice "/$R/compare?base=main&head=feat1")" "Add b"
check "same branch as base -> refused"          is "$(post alice /$R/pulls/new -d base=main -d head=main -d title=x -d body=)" 400
P1=$(new_pr alice feat1 "Add b" "Fixes #$I")
check "PR created"                              test -n "$P1"
check "PR and issue share numbering"            is "$P1" "$((I+1))"
check "refs/pull/N/head pinned"                 is "$(bare $R rev-parse refs/pull/$P1/head)" "$(bare $R rev-parse feat1)"
P=$(page alice /$R/pulls/$P1)
check "conversation: open + wants to merge"     bash -c "grep -q '● open' <<<'$(echo "$P" | tr -d "'")' && grep -q 'wants to merge' <<<'$(echo "$P" | tr -d "'")'"
check "  fast-forwardable"                      has "$P" "Can be fast-forwarded"
check "files tab shows b.txt"                   has "$(page anon /$R/pulls/$P1/files)" "b.txt"
check "commits tab shows the commit"            has "$(page anon /$R/pulls/$P1/commits)" "Add b"
check "PR shows in pulls list, not issues"      bash -c "curl -s $B/$R/pulls | grep -q 'Add b' && ! curl -s $B/$R/issues | grep -q '>Add b<'"
check "stale expected_head refused"             is "$(post alice /$R/pulls/$P1/merge -d strategy=merge -d expected_head=$(printf '0%.0s' {1..40}))" 400
check "bob (read) can't merge -> 404"           is "$(post bob /$R/pulls/$P1/merge -d strategy=merge -d expected_head=$(head_of $P1))" 404
check "merge (merge commit)"                    is "$(merge $P1 merge)" 303
check "  main tip has 2 parents"                is "$(bare $R rev-list --parents -n1 main | wc -w)" 3
check "  merge message names the PR"            bash -c "git --git-dir=$DATA/repos/$R.git log -1 --format=%s main | grep -q 'Merge pull request #$P1 from feat1'"
check "  PR merged pill + event"                bash -c "p=\$(curl -s $B/$R/pulls/$P1); grep -q '⇄ merged' <<<\"\$p\" && grep -q 'merged commit' <<<\"\$p\""
check "  'Fixes #I' closed the issue via PR"    bash -c "p=\$(curl -s $B/$R/issues/$I); grep -q '✓ closed' <<<\"\$p\" && grep -q 'closed this via' <<<\"\$p\""
check "  files tab still shows the diff"        has "$(page anon /$R/pulls/$P1/files)" "b.txt"
check "delete head branch"                      is "$(post alice /$R/pulls/$P1/delete-branch)" 303
check "  feat1 gone, pinned ref kept"           bash -c "! git --git-dir=$DATA/repos/$R.git rev-parse -q --verify refs/heads/feat1 && git --git-dir=$DATA/repos/$R.git rev-parse -q --verify refs/pull/$P1/head"

echo "== fork PR, revisions, reviews, inline comments"
check "bob forks"                               is "$(post bob /$R/fork -d owner=bob -d name=$REPO-fork)" 303
as bob git clone -q $S/$F.git bw && cd bw
git checkout -qb fix; printf 'line1\nline2 fixed\nline3\n' > a.txt; as bob git commit -qam "Fix line 2"
check "bob pushes fix to his fork"              as bob git push -q origin fix
cd ..
check "compare with fork head"                  has "$(page bob "/$R/compare?base=main&head=$F:fix")" "Fix line 2"
check "  no scratch refs left behind"           is "$(bare $R for-each-ref refs/loom | wc -l)" 0
check "head from unrelated repo refused"        is "$(post bob /$R/pulls/new -d base=main -d head=alice/slate:master -d title=x -d body=)" 400
P2=$(new_pr bob "$F:fix" "Fix line 2" "please review")
check "bob opens fork PR"                       test -n "$P2"
check "  head pinned in base repo"              is "$(bare $R rev-parse refs/pull/$P2/head)" "$(bare $F rev-parse fix)"
check "  refs/loom never advertised"            bash -c "! git ls-remote $B/$R.git | grep -q refs/loom"
cd bw; echo more >> a.txt; as bob git commit -qam "More"; as bob git push -q origin fix; cd ..
check "push to fork branch -> revision v2"      bash -c "p=\$(curl -s $B/$R/pulls/$P2); grep -q 'pushed 1 commit' <<<\"\$p\" && grep -q '>v2<' <<<\"\$p\""
check "  pinned ref followed"                   is "$(bare $R rev-parse refs/pull/$P2/head)" "$(bare $F rev-parse fix)"
cd bw; as bob git commit -q --amend -m "More (amended)"; as bob git push -q -f origin fix; cd ..
check "force-push -> v3 flagged"                bash -c "curl -s $B/$R/pulls/$P2 | grep -q 'force-pushed'"
check "revisions page with range-diff"          has "$(page anon "/$R/pulls/$P2/revisions?from=2&to=3")" "v2 → v3"
check "  range-diff mentions amended subject"   has "$(page anon "/$R/pulls/$P2/revisions?from=2&to=3")" "More (amended)"
check "inline comment on a changed line"        is "$(post alice /$R/pulls/$P2/files/comments -d path=a.txt -d side=R -d line=2 --data-urlencode 'body=nice fix')" 303
check "  shown inline in files tab"             has "$(page anon /$R/pulls/$P2/files)" "nice fix"
check "  and in the conversation"               has "$(page anon /$R/pulls/$P2)" "a.txt:2"
check "comment on a line not in diff -> 400"    is "$(post alice /$R/pulls/$P2/files/comments -d path=a.txt -d side=R -d line=99 -d body=x)" 400
check "comment with bad side -> 400"            is "$(post alice /$R/pulls/$P2/files/comments -d path=a.txt -d side=X -d line=2 -d body=x)" 400
RC=$(page anon /$R/pulls/$P2/files | grep -oE 'id="rc[0-9]+"' | head -1 | grep -oE '[0-9]+')
check "bob can't delete alice's inline comment" is "$(post bob /$R/pulls/$P2/files/comments/$RC/delete)" 400
check "?at= opens a comment form"               has "$(page alice "/$R/pulls/$P2/files?at=a.txt:R:2")" 'name="line" value="2"'
check "bob can't approve his own PR"            is "$(post bob /$R/pulls/$P2/review -d state=approve -d body=)" 400
check "change request needs text"               is "$(post alice /$R/pulls/$P2/review -d state=changes -d body=)" 400
check "alice requests changes"                 is "$(post alice /$R/pulls/$P2/review -d state=changes --data-urlencode 'body=please split')" 303
check "  shown + counted"                       bash -c "p=\$(curl -s $B/$R/pulls/$P2); grep -q 'requested changes' <<<\"\$p\" && grep -q '1 requesting changes' <<<\"\$p\""
check "alice approves (verdict replaces)"      is "$(post alice /$R/pulls/$P2/review -d state=approve -d body=)" 303
check "  1 approval, no change requests"        bash -c "p=\$(curl -s $B/$R/pulls/$P2); grep -q '1 approval' <<<\"\$p\" && ! grep -q 'requesting changes' <<<\"\$p\""

echo "== require-PR protection, squash merge"
check "protect main: PRs only"                  is "$(post alice /$R/settings/protect -d branch=main -d require_pr=1)" 200
cd w; git checkout -q main; as alice git pull -q $S/$R.git main; echo direct > d.txt; git add d.txt; as alice git commit -qm direct
check "direct push to main refused"             bash -c "! (GIT_SSH_COMMAND='ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT' git push -q $S/$R.git main 2>&1)"
check "push to refs/pull/* refused"             bash -c "! (GIT_SSH_COMMAND='ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT' git push -q $S/$R.git HEAD:refs/pull/$P2/head 2>&1)"
git reset -q --hard HEAD~1; cd ..
check "squash merge via web (bypasses rule)"    is "$(merge $P2 squash)" 303
check "  one new commit on main, 1 parent"      is "$(bare $R rev-list --parents -n1 main | wc -w)" 2
check "  author bob, committer alice"          is "$(bare $R log -1 --format='%an/%cn' main)" "bob/alice"
check "  message is 'title (#N)'"               is "$(bare $R log -1 --format=%s main)" "Fix line 2 (#$P2)"
check "  PR marked merged"                      has "$(page anon /$R/pulls/$P2)" "⇄ merged"
post alice /$R/settings/protect/remove -d branch=main >/dev/null

echo "== conflicts"
cd w; as alice git pull -q $S/$R.git main
git checkout -qb c1; sed -i 's/line3/line3 from c1/' a.txt; as alice git commit -qam c1; as alice git push -q $S/$R.git c1
git checkout -q main; git checkout -qb c2; sed -i 's/line3/line3 from c2/' a.txt; as alice git commit -qam c2; as alice git push -q $S/$R.git c2; cd ..
PC1=$(new_pr alice c1 "c1" ""); PC2=$(new_pr alice c2 "c2" "")
check "merge c1"                                is "$(merge $PC1 merge)" 303
check "c2 now shows conflicts in a.txt"         bash -c "p=\$(curl -s -b $W/alice.jar $B/$R/pulls/$PC2); grep -q 'Conflicts with' <<<\"\$p\" && grep -q '<code>a.txt</code>' <<<\"\$p\""
check "merging c2 refused"                      is "$(merge $PC2 merge)" 400
check "  main unchanged by the failed merge"    bash -c "git --git-dir=$DATA/repos/$R.git log -1 --format=%s main | grep -q 'Merge pull request #$PC1'"

echo "== rebase merge"
cd w; git checkout -q main; as alice git pull -q $S/$R.git main
git checkout -qb r1 HEAD~1; echo r > r.txt; git add r.txt; as alice git commit -qm "r1 one"; echo r2 >> r.txt; as alice git commit -qam "r1 two"; as alice git push -q $S/$R.git r1; cd ..
PR1=$(new_pr alice r1 "rebase me" "")
OLD=$(bare $R rev-parse main)
check "rebase merge"                            is "$(merge $PR1 rebase)" 303
check "  main is linear: 2 new commits on top"  bash -c "[ \$(git --git-dir=$DATA/repos/$R.git rev-list $OLD..main | wc -l) = 2 ] && [ \$(git --git-dir=$DATA/repos/$R.git rev-list --merges $OLD..main | wc -l) = 0 ]"
check "  replayed commits on old tip"           is "$(bare $R rev-parse main~2)" "$OLD"
check "  PR merged though head isn't ancestor"  has "$(page anon /$R/pulls/$PR1)" "⇄ merged"

echo "== merged by push, close/reopen"
cd w; git checkout -q main; as alice git pull -q $S/$R.git main; git checkout -qb p1; echo p > p.txt; git add p.txt; as alice git commit -qm p1; as alice git push -q $S/$R.git p1; cd ..
PP=$(new_pr alice p1 "merged by push" "")
cd w; git checkout -q main; as alice git merge -q --no-ff p1 -m "local merge"; as alice git push -q $S/$R.git main; cd ..
check "pushing a merge marks the PR merged"     bash -c "p=\$(curl -s $B/$R/pulls/$PP); grep -q '⇄ merged' <<<\"\$p\""
check "  merged PR can't be reopened"           is "$(post alice /$R/pulls/$PP/comments -d body= -d action=reopen)" 400
cd bw; git checkout -qb another; echo x > x.txt; git add x.txt; as bob git commit -qm another; as bob git push -q origin another; cd ..
PX=$(new_pr bob "$F:another" "to close" "")
check "bob closes his PR without merging"       is "$(post bob /$R/pulls/$PX/comments -d body= -d action=close)" 303
check "  'closed without merging'"              has "$(page anon /$R/pulls/$PX)" "Closed without merging"
check "  closed list shows merged + closed"     bash -c "p=\$(curl -s '$B/$R/pulls?state=closed'); grep -q 'title=\"merged\"' <<<\"\$p\" && grep -q 'to close' <<<\"\$p\""
check "bob reopens it"                          is "$(post bob /$R/pulls/$PX/comments -d body= -d action=reopen)" 303
check "duplicate PR for same head -> existing"  bash -c "[ \"\$(curl -s -o /dev/null -w '%{redirect_url}' -b $W/bob.jar -H 'Origin: $B' -d base=main -d head=$F:another -d title=dup -d body= $B/$R/pulls/new | grep -oE '[0-9]+\$')\" = $PX ]"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
