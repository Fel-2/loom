#!/usr/bin/env bash
# Milestone 4: issues, comments, labels, milestones, assignees, references,
# commit-driven closing. Re-runnable (unique titles, numbers read from redirects).
# Git runs with an empty config: no credential helpers, never any prompt/GUI.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m4.XXXXXX")
trap 'rm -rf "$W"' EXIT
cd "$W" || exit 1
RUN=$$

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_SSH_COMMAND="ssh -i $KEYS/alice -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT"
S="ssh://git@127.0.0.1:$SSH_PORT"

pass=0 fail=0
check() { local name=$1; shift; printf '%-60s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }

login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
get() { curl -s -o "$W/last.html" -w '%{http_code}' -b "$W/$1.jar" "$B$2"; }
page() { curl -s -b "$W/$1.jar" "$B$2"; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
# new_issue <user> <owner/repo> <title> <body> -> prints number
new_issue() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode "title=$3" --data-urlencode "body=$4" "$B/$2/issues/new" | grep -oE '[0-9]+$'; }

login alice alice-test-pw; login bob bob-test-pw1; touch "$W/anon.jar"
R=alice/ytm-tui

echo "== create & view"
check "default labels on migrated repo"          has "$(page anon /$R/labels)" "good first issue"
I1=$(new_issue bob $R "bob bug $RUN" "Steps: <script>alert(1)</script> thanks @alice and @nobody-here")
check "bob opens an issue"                       test -n "$I1"
I2=$(new_issue alice $R "alice task $RUN" "Related to #$I1 and $(git --git-dir=$DATA/repos/$R.git rev-parse --short=10 HEAD)")
check "numbers are sequential"                   is "$I2" "$((I1+1))"
P=$(page anon /$R/issues/$I1)
check "anon can read the issue"                  has "$P" "bob bug $RUN"
check "raw HTML in body is escaped"              lacks "$P" "<script>alert"
check "@alice links, @nobody-here doesn't"      bash -c "grep -q 'class=\"ref mention\" href=\"/alice\"' <<<'$(echo "$P" | tr -d "'")' && ! grep -q 'href=\"/nobody-here\"' <<<'$(echo "$P" | tr -d "'")'"
P2=$(page anon /$R/issues/$I2)
check "#N links to the issue"                    has "$P2" "href=\"/$R/issues/$I1\""
check "short sha links to the commit"            has "$P2" "class=\"ref sha\""
check "mention event on the target issue"        has "$(page anon /$R/issues/$I1)" "mentioned this in"
check "anon 'new issue' -> login"                is "$(curl -s -o /dev/null -w '%{http_code}' $B/$R/issues/new)" 303
check "empty title rejected"                     is "$(post bob /$R/issues/new -d title=%20 -d body=x)" 400
check "256+ char title rejected"                 is "$(post bob /$R/issues/new --data-urlencode "title=$(printf 'x%.0s' {1..300})" -d body=)" 400
check "list shows open count"                    has "$(page anon /$R/issues)" "bob bug $RUN"

echo "== comments"
check "bob comments"                             is "$(post bob /$R/issues/$I1/comments --data-urlencode 'body=first comment' -d action=comment)" 303
CB=$(page anon /$R/issues/$I1 | grep -oE 'id="c[0-9]+"' | tail -1 | grep -oE '[0-9]+')
check "alice comments"                          is "$(post alice /$R/issues/$I1/comments --data-urlencode 'body=maintainer reply' -d action=comment)" 303
CM=$(page anon /$R/issues/$I1 | grep -oE 'id="c[0-9]+"' | tail -1 | grep -oE '[0-9]+')
check "bob edits own comment"                    is "$(post bob /$R/comments/$CB/edit --data-urlencode 'body=first comment (edited)')" 303
check "  shows edited text"                      has "$(page anon /$R/issues/$I1)" "first comment (edited)"
check "bob can't edit alice's comment"          is "$(post bob /$R/comments/$CM/edit -d body=hijack)" 400
check "bob can't delete alice's comment"        is "$(post bob /$R/comments/$CM/delete)" 400
check "comment id from another repo -> 404"      is "$(post bob /alice/tokmon/comments/$CB/edit -d body=x)" 404
check "empty comment rejected"                   is "$(post bob /$R/issues/$I1/comments -d body= -d action=comment)" 400
check "admin deletes bob's comment"              is "$(post alice /$R/comments/$CB/delete)" 303
check "  comment gone"                           lacks "$(page anon /$R/issues/$I1)" "first comment (edited)"
check "cross-site comment refused"               is "$(curl -s -o /dev/null -w '%{http_code}' -b $W/bob.jar -H 'Origin: https://evil.example' -d body=x -d action=comment $B/$R/issues/$I1/comments)" 403

echo "== open / close"
check "bob can't close alice's issue"           is "$(post bob /$R/issues/$I2/comments -d body= -d action=close)" 400
check "bob closes his own (with comment)"        is "$(post bob /$R/issues/$I1/comments --data-urlencode 'body=never mind' -d action=close)" 303
check "  closed pill + event"                    bash -c "p=\$(curl -s $B/$R/issues/$I1); grep -q '✓ closed' <<<\"\$p\" && grep -q 'closed this' <<<\"\$p\""
check "  listed under closed"                    has "$(page anon "/$R/issues?state=closed")" "bob bug $RUN"
check "alice reopens it"                        is "$(post alice /$R/issues/$I1/comments -d body= -d action=reopen)" 303
check "  reopened event"                         has "$(page anon /$R/issues/$I1)" "reopened this"
check "bob renames own issue"                    is "$(post bob /$R/issues/$I1/edit --data-urlencode "title=bob bug $RUN v2" --data-urlencode 'body=updated body')" 303
check "  rename event"                           has "$(page anon /$R/issues/$I1)" "changed the title"

echo "== triage"
BUG=$(page alice /$R/labels | grep -oE '/labels/[0-9]+"' | head -1 | grep -oE '[0-9]+')
ALL=$(page alice /$R/labels | grep -oE '/labels/[0-9]+"' | grep -oE '[0-9]+' | head -2 | tr '\n' ' ')
L1=$(echo $ALL | cut -d' ' -f1); L2=$(echo $ALL | cut -d' ' -f2)
check "bob (read) can't label -> 404"            is "$(post bob /$R/issues/$I1/labels -d label=$L1)" 404
check "alice applies two labels"                is "$(post alice /$R/issues/$I1/labels -d label=$L1 -d label=$L2)" 303
check "  'added label' events"                   bash -c "[ \$(curl -s $B/$R/issues/$I1 | grep -o 'added label' | wc -l) -ge 2 ]"
LN=$(page anon /$R/labels | grep -oE "href=\"/$R/issues\?label=[^\"]+\"" | head -1 | sed 's/.*label=//; s/"$//')
check "filter by label finds it"                 has "$(page anon "/$R/issues?label=$LN")" "bob bug $RUN v2"
check "alice removes one label"                 is "$(post alice /$R/issues/$I1/labels -d label=$L1)" 303
check "  'removed label' event"                  has "$(page anon /$R/issues/$I1)" "removed label"
check "assign bob"                               is "$(post alice /$R/issues/$I1/assignees -d add=bob)" 303
check "  assigned event + filter"                bash -c "curl -s $B/$R/issues/$I1 | grep -q 'assigned <a href=\"/bob\"' && curl -s '$B/$R/issues?assignee=bob' | grep -q 'bob bug $RUN v2'"
check "assign unknown user -> 400"               is "$(post alice /$R/issues/$I1/assignees -d add=ghost$RUN)" 400
SI=$(new_issue alice alice/secret "secret issue $RUN" "x")
check "assign user without access -> 400"        is "$(post alice /alice/secret/issues/$SI/assignees -d add=bob)" 400
check "unassign bob"                             is "$(post alice /$R/issues/$I1/assignees -d remove=bob)" 303

echo "== milestones"
check "bad due date rejected"                    is "$(post alice /$R/milestones -d "title=v$RUN" -d description= -d due=2026-13-40)" 400
check "create milestone with due date"           is "$(post alice /$R/milestones -d "title=v$RUN" -d description=first -d due=2026-12-31)" 200
# Pick the milestone by its title: pages accumulate milestones across runs.
MID=$(page alice /$R/milestones | grep -oE "milestones/[0-9]+\"><input name=\"title\" value=\"v$RUN\"" | grep -oE '[0-9]+' | head -1)
check "set milestone on issue"                   is "$(post alice /$R/issues/$I1/milestone -d milestone=$MID)" 303
check "  milestone event + filter"               bash -c "curl -s $B/$R/issues/$I1 | grep -q 'added this to' && curl -s '$B/$R/issues?milestone=v$RUN' | grep -q 'bob bug $RUN v2'"
check "  progress shows 0% with 1 open"          has "$(page anon /$R/milestones)" "0% · 1 open"
check "duplicate milestone title -> 400"         is "$(post alice /$R/milestones -d "title=v$RUN" -d description= -d due=)" 400

check "delete the test milestone"                is "$(post alice /$R/milestones/$MID/delete)" 200

echo "== labels page"
check "bad color rejected"                       is "$(post alice /$R/labels -d "name=l$RUN" -d 'color=red;background:url(x)' -d description=)" 400
check "create label"                             is "$(post alice /$R/labels -d "name=l$RUN" -d color=%23123abc -d description=test)" 200
# The label's own edit form (pages are one line: grep -B/-A context is meaningless).
NL=$(page alice /$R/labels | grep -oE "labels/[0-9]+\"><input name=\"name\" value=\"l$RUN\"" | grep -oE '[0-9]+' | head -1)
check "duplicate label (case-insensitive) -> 400" is "$(post alice /$R/labels -d "name=L$RUN" -d color=%23123abc -d description=)" 400
check "bob can't create labels -> 404"           is "$(post bob /$R/labels -d name=x -d color=%23000000 -d description=)" 404
check "delete label (exactly that one)"          bash -c "[ -n '$NL' ] && [ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/alice.jar -H 'Origin: $B' -X POST $B/$R/labels/$NL/delete) = 200 ] && ! curl -s $B/$R/labels | grep -q '>l$RUN<' && curl -s $B/$R/labels | grep -q '>bug<'"

echo "== privacy"
check "private issue 404 for bob"                is "$(get bob /alice/secret/issues/$SI)" 404
check "private issue list 404 for anon"          is "$(get anon /alice/secret/issues)" 404
X=$(new_issue alice alice/secret "leaky $RUN" "see $R#$I2")
check "private mention hidden from anon"         lacks "$(page anon /$R/issues/$I2)" "alice/secret#$X"
check "  ...but shown to alice"                 has "$(page alice /$R/issues/$I2)" "alice/secret#$X"

echo "== commits"
C=$(new_issue bob $R "fix me $RUN" "to be fixed by a commit")
D=$(new_issue bob $R "mention me $RUN" "to be mentioned by a commit")
git clone -q "$S/$R.git" w && cd w
BR=$(git symbolic-ref --short HEAD)
git commit -q --allow-empty -m "Repair the thing" -m "Fixes #$C, see #$D"
check "push to default branch"                   git push -q origin HEAD
SHA=$(git rev-parse HEAD)
check "  'Fixes #C' closed the issue"            bash -c "p=\$(curl -s $B/$R/issues/$C); grep -q '✓ closed' <<<\"\$p\" && grep -q 'closed this in' <<<\"\$p\""
check "  '#D' added a commit reference"          has "$(page anon /$R/issues/$D)" "referenced this in"
check "  ...and D stays open"                    has "$(page anon /$R/issues/$D)" "● open"
check "same commit on another branch: no dupes"  bash -c "git push -q origin HEAD:refs/heads/dup-$RUN && [ \$(curl -s $B/$R/issues/$D | grep -o 'referenced this in' | wc -l) = 1 ]"
git checkout -q -b side-$RUN; git commit -q --allow-empty -m "Fixes #$D on a side branch"
check "fix on non-default branch: no close"      bash -c "git push -q origin HEAD && curl -s $B/$R/issues/$D | grep -q '● open'"
git push -q origin :side-$RUN :dup-$RUN 2>/dev/null; cd ..

echo "== archived"
post alice /alice/tokmon/settings/general -d description= -d archived=1 >/dev/null
check "archived: new issue blocked"              is "$(get alice /alice/tokmon/issues/new)" 404
post alice /alice/tokmon/settings/general -d description= >/dev/null

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
