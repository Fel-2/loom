#!/usr/bin/env bash
# Milestone 6: webhooks (signed, retried, logged), commit statuses + required
# checks, REST API with tokens, notifications/watching, activity, search.
# Fresh repo per run; a local receiver (127.0.0.1:3999) records deliveries.
# Git runs with an empty config: no credential helpers, never any prompt/GUI.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
RECV=${RECV:-$HOME/.cache/loom-bench/hookrecv-tool/recv.py}
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-m6.XXXXXX")
RUN=$$
REPO=m6r$RUN
R=alice/$REPO
mkdir -p "$W/hooks"
python3 -I "$RECV" 3999 "$W/hooks" & RPID=$!
cleanup() {
  kill $RPID 2>/dev/null
  post alice /$R/settings/delete -d confirm=$R >/dev/null 2>&1
  rm -rf "$W"
}
trap cleanup EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
S="ssh://git@127.0.0.1:$SSH_PORT"
as() { local who=$1; shift; GIT_SSH_COMMAND="ssh -i $KEYS/$who -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT" \
  GIT_AUTHOR_NAME=$who GIT_AUTHOR_EMAIL=$who@test GIT_COMMITTER_NAME=$who GIT_COMMITTER_EMAIL=$who@test "$@"; }

pass=0 fail=0
check() { local name=$1; shift; printf '%-62s' "$name"; if out=$("$@" 2>&1); then echo ok; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
login() { curl -s -o /dev/null -c "$W/$1.jar" -d "name=$1&password=$2" "$B/login"; }
post() { local who=$1 path=$2; shift 2; curl -s -X POST -o "$W/last.html" -w '%{http_code}' -b "$W/$who.jar" -H "Origin: $B" "$@" "$B$path"; }
page() { curl -s -b "$W/$1.jar" "$B$2"; }
is() { [ "$1" = "$2" ] || { echo "got '$1', want '$2'"; return 1; }; }
has() { grep -qF -- "$2" <<<"$1" || { echo "missing: $2"; return 1; }; }
lacks() { ! grep -qF -- "$2" <<<"$1" || { echo "unexpected: $2"; return 1; }; }
new_issue() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode "title=$3" --data-urlencode "body=$4" "$B/$2/issues/new" | grep -oE '[0-9]+$'; }
new_pr() { curl -s -o /dev/null -w '%{redirect_url}' -b "$W/$1.jar" -H "Origin: $B" --data-urlencode base=main --data-urlencode "head=$2" --data-urlencode "title=$3" --data-urlencode body= "$B/$R/pulls/new" | grep -oE '[0-9]+$'; }
head_of() { page alice /$R/pulls/$1 | grep -oE 'name="expected_head" value="[0-9a-f]{40}"' | grep -oE '[0-9a-f]{40}'; }
token() { curl -s -b "$W/$1.jar" -H "Origin: $B" -d "title=t$RUN" "$B/settings/tokens" | grep -oE 'loom_[0-9a-f]{64}' | head -1; }
api() { local tok=$1 method=$2 path=$3; shift 3; curl -s -o "$W/api.json" -w '%{http_code}' -X "$method" ${tok:+-H "Authorization: Bearer $tok"} -H 'Content-Type: application/json' "$@" "$B/api/v1$path"; }
# wait_hook <event> [substring]: a delivery of that event (containing substring) arrives
wait_hook() {
  for _ in $(seq 40); do
    python3 -I - "$W/hooks" "$1" "${2:-}" <<'PY' && return 0
import json, os, sys
d, ev, sub = sys.argv[1:4]
for f in sorted(os.listdir(d)):
    r = json.load(open(os.path.join(d, f)))
    h = {k.lower(): v for k, v in r["headers"].items()}
    if h.get("x-github-event") == ev and sub in r["body"]:
        sys.exit(0)
sys.exit(1)
PY
    sleep 0.25
  done
  echo "no '$1' delivery containing '${2:-}'"; return 1
}

login alice alice-test-pw; login bob bob-test-pw1
check "create $R"                                is "$(post alice /new -d owner=alice -d name=$REPO -d description=m6)" 303
as alice git init -q -b main w && cd w && printf 'needle_%s here\n' $RUN > code.txt && git add . && as alice git commit -qm initial && cd ..

echo "== webhooks"
check "invalid hook URL -> 400"                  is "$(post alice /$R/settings/hooks -d url=ftp://x -d event=push)" 400
check "bob can't add hooks -> 404"               is "$(post bob /$R/settings/hooks -d url=http://127.0.0.1:3999/ok -d event=push)" 404
check "add signed hook (all events)"             is "$(post alice /$R/settings/hooks -d url=http://127.0.0.1:3999/ok -d secret=s3cret $(for e in push issues issue_comment pull_request pull_request_review status; do echo -d event=$e; done))" 200
check "  ping delivered"                         wait_hook ping
check "  HMAC signature verifies"                python3 -I - "$W/hooks" <<'PY'
import hashlib, hmac, json, os, sys
d = sys.argv[1]
for f in sorted(os.listdir(d)):
    r = json.load(open(os.path.join(d, f)))
    h = {k.lower(): v for k, v in r["headers"].items()}
    if h.get("x-github-event") == "ping":
        want = "sha256=" + hmac.new(b"s3cret", r["body"].encode(), hashlib.sha256).hexdigest()
        sys.exit(0 if h.get("x-hub-signature-256") == want else 1)
sys.exit(1)
PY
cd w; check "push (ssh)"                          as alice git push -q $S/$R.git main; cd ..
check "  push event with ref + commit"           wait_hook push '"ref":"refs/heads/main"'
I=$(new_issue alice $R "hooked issue $RUN" "body with uniqueword$RUN")
check "  issues/opened delivered"                wait_hook issues '"action":"opened"'
check "  issue_comment delivered"                bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/alice.jar -H 'Origin: $B' --data-urlencode 'body=comment with commentword$RUN' -d action=comment $B/$R/issues/$I/comments) = 303 ]"
check "    (payload has the comment)"            wait_hook issue_comment "commentword$RUN"
check "add failing hook"                         is "$(post alice /$R/settings/hooks -d url=http://127.0.0.1:3999/fail -d event=issues)" 200
FH=$(page alice /$R/settings | grep -oE "/settings/hooks/[0-9]+\"[^>]*>http://127.0.0.1:3999/fail" | grep -oE 'hooks/[0-9]+' | grep -oE '[0-9]+')
new_issue alice $R "trigger failure $RUN" "" >/dev/null
check "  failed delivery logged (500, retrying)" bash -c "for i in \$(seq 40); do curl -s -b $W/alice.jar $B/$R/settings/hooks/$FH | grep -q 'verdict bad\">500' && exit 0; sleep 0.25; done; exit 1"
DID=$(page alice /$R/settings/hooks/$FH | grep -oE "deliveries/[0-9]+/redeliver" | head -1 | grep -oE '[0-9]+')
check "  redeliver queues a new attempt"         is "$(post alice /$R/settings/hooks/$FH/deliveries/$DID/redeliver)" 303

echo "== API & tokens"
MT=$(token alice); BT=$(token bob)
check "tokens issued"                            bash -c "[ -n '$MT' ] && [ -n '$BT' ]"
check "GET /user with token"                     bash -c "[ \$(curl -s -H 'Authorization: Bearer $MT' $B/api/v1/user | python3 -I -c 'import json,sys; print(json.load(sys.stdin)[\"login\"])') = alice ]"
check "bad token -> 401"                         is "$(api loom_bogus GET /user)" 401
check "no token -> 401 on /user"                 is "$(api '' GET /user)" 401
check "GET repo (anon, public)"                  bash -c "[ \$(curl -s $B/api/v1/repos/$R | python3 -I -c 'import json,sys; print(json.load(sys.stdin)[\"full_name\"])') = $R ]"
check "private repo anon -> 404"                 is "$(api '' GET /repos/alice/secret)" 404
check "unknown API path -> JSON 404"             bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' $B/api/v1/nope) = 404 ] && curl -s $B/api/v1/nope | grep -q message"
check "create issue via API -> 201"              is "$(api $BT POST /repos/$R/issues -d "{\"title\":\"api issue $RUN\",\"body\":\"from api\"}")" 201
AI=$(python3 -I -c 'import json; print(json.load(open("'$W'/api.json"))["number"])' 2>/dev/null)
check "comment via API -> 201"                   is "$(api $BT POST /repos/$R/issues/$AI/comments -d '{"body":"api comment"}')" 201
check "list issues via API includes it"          bash -c "curl -s $B/api/v1/repos/$R/issues | grep -q 'api issue $RUN'"
check "empty title -> 422"                       is "$(api $BT POST /repos/$R/issues -d '{"title":" "}')" 422

echo "== statuses & required checks"
SHA=$(git -C w rev-parse HEAD)
check "status by owner token -> 201"             is "$(api $MT POST /repos/$R/statuses/$SHA -d '{"state":"success","context":"ci/test","description":"ok","target_url":"http://ci/1"}')" 201
check "bob (read) -> 403"                        is "$(api $BT POST /repos/$R/statuses/$SHA -d '{"state":"success","context":"x"}')" 403
check "bad state -> 422"                         is "$(api $MT POST /repos/$R/statuses/$SHA -d '{"state":"great"}')" 422
check "javascript: target_url -> 422"            is "$(api $MT POST /repos/$R/statuses/$SHA -d '{"state":"success","target_url":"javascript:alert(1)"}')" 422
check "combined status by branch name"           bash -c "curl -s $B/api/v1/repos/$R/commits/main/status | grep -q '\"state\":\"success\"'"
check "  status webhook delivered"               wait_hook status '"context":"ci/test"'
check "  shown on commit page"                   has "$(page anon /$R/commit/$SHA)" "ci/test"
check "protect main: require ci/jenkins"         is "$(post alice /$R/settings/protect -d branch=main -d required_checks=ci/jenkins)" 200
cd w; git checkout -qb feat; echo f > f.txt; git add f.txt; as alice git commit -qm feat; as alice git push -q $S/$R.git feat; cd ..
P=$(new_pr alice feat "feat $RUN")
check "  pull_request/opened delivered"          wait_hook pull_request '"action":"opened"'
check "  PR shows required check missing"        has "$(page alice /$R/pulls/$P)" "Required checks must pass first: ci/jenkins"
check "  merge refused without the check"        is "$(post alice /$R/pulls/$P/merge -d strategy=merge -d expected_head=$(head_of $P))" 400
HS=$(git -C w rev-parse feat)
api $MT POST /repos/$R/statuses/$HS -d '{"state":"pending","context":"ci/jenkins"}' >/dev/null
check "  pending still blocks"                   has "$(page alice /$R/pulls/$P)" "Required checks must pass first"
api $MT POST /repos/$R/statuses/$HS -d '{"state":"success","context":"ci/jenkins","target_url":"http://jenkins/42"}' >/dev/null
check "  success unblocks; merge works"          is "$(post alice /$R/pulls/$P/merge -d strategy=merge -d expected_head=$(head_of $P))" 303
check "  merged webhook: closed + merged:true"   wait_hook pull_request '"merged":true'

echo "== notifications & watching"
BI=$(new_issue bob $R "bob reports $RUN" "")
check "owner (auto-watching) notified"           has "$(page alice /notifications)" "bob reports $RUN"
check "  bell shows unread count"                bash -c "curl -s -b $W/alice.jar $B/ | grep -q 'class=\"unread\"'"
check "  opening the issue marks it read"        bash -c "curl -s -o /dev/null -b $W/alice.jar $B/$R/issues/$BI; ! curl -s -b $W/alice.jar $B/notifications | grep -q 'bob reports $RUN'"
MI=$(new_issue alice $R "ping bob $RUN" "")
check "bob not watching: not notified"           lacks "$(page bob /notifications)" "ping bob $RUN"
post alice /$R/issues/$MI/comments --data-urlencode "body=hey @bob look" -d action=comment >/dev/null
check "@mention notifies bob (reason mention)"   bash -c "p=\$(curl -s -b $W/bob.jar $B/notifications); grep -q 'ping bob $RUN' <<<\"\$p\" && grep -q '>mention<' <<<\"\$p\""
post bob /notifications/read >/dev/null
post alice /$R/issues/$MI/assignees -d add=bob >/dev/null
check "assignment notifies bob"                  bash -c "curl -s -b $W/bob.jar $B/notifications | grep -q '>assigned<'"
check "bob watches the repo"                     is "$(post bob /$R/watch)" 303
WI=$(new_issue alice $R "for watchers $RUN" "")
check "  watcher notified of new issue"          has "$(page bob /notifications)" "for watchers $RUN"
post bob /$R/watch >/dev/null
UI=$(new_issue alice $R "after unwatch $RUN" "")
check "  unwatched: no notification"             lacks "$(page bob '/notifications?all=1')" "after unwatch $RUN"
check "mark all read"                            bash -c "[ \$(curl -s -o /dev/null -w '%{http_code}' -b $W/bob.jar -H 'Origin: $B' -X POST $B/notifications/read) = 303 ] && curl -s -b $W/bob.jar $B/notifications | grep -q 'All caught up'"
SI=$(new_issue alice alice/secret "secret ping $RUN" "@bob can you see this")
check "mention in private repo: bob not notified" lacks "$(page bob '/notifications?all=1')" "secret ping $RUN"

echo "== activity & search"
check "dashboard shows activity"                 has "$(page alice /)" "opened issue"
check "repo search finds it"                     has "$(page anon "/search?q=$REPO&type=repos")" "$REPO"
check "issue search: body word"                  has "$(page anon "/search?q=uniqueword$RUN&type=issues")" "hooked issue $RUN"
check "issue search: comment word"               has "$(page anon "/search?q=commentword$RUN&type=issues")" "hooked issue $RUN"
check "  match highlighted"                      has "$(page anon "/search?q=uniqueword$RUN&type=issues")" "<mark>"
# (the query itself is echoed in the search box: check result links, not text)
check "private issue hidden from anon"           lacks "$(page anon "/search?q=secret+ping+$RUN&type=issues")" 'href="/alice/secret/issues/'
check "  ...and from bob"                        lacks "$(page bob "/search?q=secret+ping+$RUN&type=issues")" 'href="/alice/secret/issues/'
check "  ...but found by alice"                 has "$(page alice "/search?q=secret+ping+$RUN&type=issues")" "href=\"/alice/secret/issues/$SI\""
check "FTS syntax in query doesn't break"        is "$(curl -s -o /dev/null -w '%{http_code}' "$B/search?q=%22foo%22+OR*+(+NEAR&type=issues")" 200
check "code search (git grep) finds the line"    has "$(page anon "/$R/search?q=needle_$RUN&type=code")" "code.txt:1"
check "repo-scoped issue search"                 has "$(page anon "/$R/search?q=uniqueword$RUN&type=issues")" "hooked issue $RUN"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
