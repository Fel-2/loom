#!/bin/sh
# End-to-end smoke test for a running loom: HTTP + SSH clone/push permissions.
# Needs users alice (admin), bob, git; repos alice/ytm-tui, alice/slate,
# alice/secret (private, empty), bob/bobs (empty), git/git; test keys in $KEYS.
#
# Git runs with an empty config: no credential helpers, never any prompt/GUI.
set -u
B=${LOOM_TEST_URL:-http://127.0.0.1:3917}
HOSTPORT=${B#http://}
SSH_PORT=${LOOM_TEST_SSH_PORT:-2917}
KEYS=${KEYS:-$HOME/.cache/loom-bench/keys}
DATA=${LOOM_DATA:-$HOME/Projects/loom/data}
W=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-smoke.XXXXXX")
trap 'rm -rf "$W"' EXIT
cd "$W" || exit 1

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_ASKPASS=/bin/false SSH_ASKPASS=/bin/false GCM_INTERACTIVE=never
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
G="git -c credential.helper="
ssh_as() { echo "ssh -i $KEYS/$1 -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -p $SSH_PORT"; }
SSHB="ssh://git@127.0.0.1:$SSH_PORT"

pass=0 fail=0
ok() { printf '%-56s' "$1"; shift; if out=$("$@" 2>&1); then echo "ok"; pass=$((pass+1)); else echo "FAIL: $(echo "$out" | tail -1)"; fail=$((fail+1)); fi; }
no() { printf '%-56s' "$1"; shift; if out=$("$@" 2>&1); then echo "FAIL: allowed"; fail=$((fail+1)); else echo "ok (refused)"; pass=$((pass+1)); fi; }

echo "== http"
ok "anon clone public"                     $G clone -q "$B/alice/ytm-tui.git" a1
ok "anon clone public, no .git suffix"     $G clone -q "$B/alice/slate" a2
ok "anon shallow clone git/git, proto v2"  $G -c protocol.version=2 clone -q --depth 20 "$B/git/git.git" a3
no "anon clone private"                    $G clone -q "$B/alice/secret.git" x1
no "anon push public"                      $G -C a1 push -q "$B/alice/ytm-tui.git" HEAD:refs/heads/anon
no "wrong password"                        $G clone -q "http://alice:nope@$HOSTPORT/alice/secret.git" x2
no "nonexistent repo"                      $G clone -q "$B/alice/nope.git" x3
ok "owner clone private (password)"        $G clone -q "http://alice:alice-test-pw@$HOSTPORT/alice/secret.git" p1
git -C p1 commit -q --allow-empty -m "http commit"
ok "owner push private (password)"         $G -C p1 push -q origin HEAD:main
no "bob clone alice's private"            $G clone -q "http://bob:bob-test-pw1@$HOSTPORT/alice/secret.git" x4
no "bob push alice's public"              $G -C a1 push -q "http://bob:bob-test-pw1@$HOSTPORT/alice/ytm-tui.git" HEAD:refs/heads/bob
git -C a1 branch -q -f master HEAD
ok "bob first push 'master' into empty bobs" $G -C a1 push -q "http://bob:bob-test-pw1@$HOSTPORT/bob/bobs.git" master
ok "bobs HEAD repointed to master"         sh -c "[ \"\$(git --git-dir=$DATA/repos/bob/bobs.git symbolic-ref HEAD)\" = refs/heads/master ]"

echo "== ssh"
ok "alice ssh clone private"              env GIT_SSH_COMMAND="$(ssh_as alice)" $G clone -q "$SSHB/alice/secret.git" s1
ok "  sees http commit"                    sh -c "git -C s1 log --oneline | grep -q 'http commit'"
git -C s1 commit -q --allow-empty -m "ssh commit"
ok "alice ssh push private"               env GIT_SSH_COMMAND="$(ssh_as alice)" $G -C s1 push -q origin HEAD:main
ok "alice ssh clone, proto v2"            env GIT_SSH_COMMAND="$(ssh_as alice)" $G -c protocol.version=2 clone -q "$SSHB/alice/ytm-tui.git" s2
ok "scp-style path without .git"           env GIT_SSH_COMMAND="$(ssh_as alice)" $G clone -q "$SSHB/alice/slate" s3
no "bob ssh clone alice's private"        env GIT_SSH_COMMAND="$(ssh_as bob)" $G clone -q "$SSHB/alice/secret.git" x5
no "bob ssh push alice's public"          env GIT_SSH_COMMAND="$(ssh_as bob)" $G -C s2 push -q origin HEAD:refs/heads/bob
ok "bob ssh push own repo"                 env GIT_SSH_COMMAND="$(ssh_as bob)" $G -C s2 push -q "$SSHB/bob/bobs.git" HEAD:refs/heads/from-ssh
no "unknown key refused"                   sh -c "ssh-keygen -q -t ed25519 -N '' -f $W/rogue && GIT_SSH_COMMAND=\"\$(echo '$(ssh_as alice)' | sed 's#$KEYS/alice#$W/rogue#')\" $G clone -q $SSHB/alice/slate.git x6"
no "shell access refused"                  sh -c "$(ssh_as alice) git@127.0.0.1 'cat /etc/passwd' | grep -q root"
no "path traversal refused"                env GIT_SSH_COMMAND="$(ssh_as alice)" $G clone -q "$SSHB/../../etc.git" x7
ok "  round trip: http sees ssh commit"    sh -c "$G clone -q http://alice:alice-test-pw@$HOSTPORT/alice/secret.git r1 && git -C r1 log --oneline | grep -q 'ssh commit'"

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
