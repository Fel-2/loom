#!/usr/bin/env bash
# Side-by-side benchmark: loom vs Forgejo on identical data (git/git with
# 83k commits + ytm-tui; 20 issues x 3 comments; one PR). Each server is
# measured alone while the other idles. Prints a report and saves it.
#
# Needs: loom on :3917 (pgrep -x loom), forgejo-bench container on :3918,
# bench data created as in bench/README.md.
set -u
LG=${LG:-$HOME/Projects/loom/bench/loadgen/target/release/loadgen}
OUT=${OUT:-$HOME/Projects/loom/bench/results/$(date +%Y%m%d-%H%M%S).txt}
CONC=${CONC:-32}
REPS=${REPS:-40}
D=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/loom-bench-run.XXXXXX")
trap 'rm -rf "$D"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0

L=http://127.0.0.1:3917
F=http://127.0.0.1:3918
SHA=$(git -c credential.helper= ls-remote $L/git/git.git refs/heads/master | cut -f1)
LPID=$(pgrep -x loom)
FPID=$(docker top forgejo-bench -eo pid,args 2>/dev/null | awk '/(gitea|forgejo) web/ && !/awk/ {print $1; exit}')
if [ "${ONLY:-}" = loom-load ]; then
  [ -n "$LPID" ] || { echo "need loom running"; exit 1; }
else
  [ -n "$LPID" ] && [ -n "$FPID" ] || { echo "need loom and forgejo-bench running"; exit 1; }
fi

# label <TAB> url — same page on each side.
cat > $D/loom.tsv <<T
repo-home	$L/git/git
tree-dir	$L/git/git/tree/master/Documentation
blob-diff.c	$L/git/git/blob/master/diff.c
raw-Makefile	$L/git/git/raw/master/Makefile
log	$L/git/git/log/master
log-page-50	$L/git/git/log/master?page=50
commit	$L/git/git/commit/$SHA
branches	$L/git/git/refs
issues	$L/git/git/issues
issue	$L/git/git/issues/1
pr	$L/git/git/pulls/21
pr-files	$L/git/git/pulls/21/files
small-repo	$L/alice/ytm-tui
user	$L/alice
api-repo	$L/api/v1/repos/git/git
api-issues	$L/api/v1/repos/git/git/issues
T
cat > $D/forgejo.tsv <<T
repo-home	$F/git/git
tree-dir	$F/git/git/src/branch/master/Documentation
blob-diff.c	$F/git/git/src/branch/master/diff.c
raw-Makefile	$F/git/git/raw/branch/master/Makefile
log	$F/git/git/commits/branch/master
log-page-50	$F/git/git/commits/branch/master?page=50
commit	$F/git/git/commit/$SHA
branches	$F/git/git/branches
issues	$F/git/git/issues
issue	$F/git/git/issues/1
pr	$F/git/git/pulls/21
pr-files	$F/git/git/pulls/21/files
small-repo	$F/alice/ytm-tui
user	$F/alice
api-repo	$F/api/v1/repos/git/git
api-issues	$F/api/v1/repos/git/git/issues
T

clones() { # base pid -> "time  peak tree RSS"
  local base=$1 pid=$2
  rm -f $D/stop $D/cpeak   # before the sampler starts (a stale stop file ended it at once)
  ( p=0; while [ ! -f $D/stop ]; do t=0; for c in $pid $(pgrep -P $pid) $(for k in $(pgrep -P $pid); do pgrep -P $k; done); do r=$(awk '/VmRSS/{print $2}' /proc/$c/status 2>/dev/null); t=$((t + ${r:-0})); done; [ $t -gt $p ] && p=$t; echo $p > $D/cpeak; sleep 0.05; done ) & local sp=$!
  local s=$(date +%s.%N); local pids=()
  for i in 1 2 3 4 5 6 7 8; do git -c credential.helper= clone -q --bare $base/alice/ytm-tui.git $D/c-small-$i & pids+=($!); done
  for i in 1 2; do git -c credential.helper= clone -q --bare $base/git/git.git $D/c-big-$i & pids+=($!); done
  local ok=0; for p in "${pids[@]}"; do wait $p && ok=$((ok+1)); done
  local e=$(date +%s.%N); touch $D/stop; wait $sp; rm -rf $D/c-*
  echo "clones ok=$ok/10 (8x ytm-tui + 2x git/git) time=$(echo "$e - $s" | bc | cut -c1-5)s peak_tree_rss_kb=$(cat $D/cpeak)"
}

bench() { # name pid urls base
  local name=$1 pid=$2 urls=$3 base=$4
  echo "################ $name (pid $pid)"
  echo "## idle (20 s)";                   $LG idle --pid $pid --secs 20
  echo "## uncontended latency (1 client, 10 reps)"
  $LG run --urls $urls --conc 1 --reps 10 --warmup 2 --pid $pid
  echo "## load ($CONC clients, $REPS reps per page)"
  $LG run --urls $urls --conc $CONC --reps $REPS --warmup 0 --pid $pid
  echo "## after load: settle 70 s, then idle (10 s)"; sleep 70; $LG idle --pid $pid --secs 10
  echo "## concurrent clones over HTTP";   clones $base $pid
  echo "## after clones: settle 70 s, then idle (10 s)"; sleep 70; $LG idle --pid $pid --secs 10
}

if [ "${ONLY:-}" = loom-load ]; then
  # Quick loom-only load phase (no Forgejo needed), for tuning.
  $LG run --urls $D/loom.tsv --conc $CONC --reps $REPS --warmup 1 --pid $LPID
  exit 0
fi

if [ "${ONLY:-}" = clones ]; then
  echo "## clones only — $(date -Is)"
  echo "loom:    $(clones $L $LPID)"; sleep 30
  echo "forgejo: $(clones $F $FPID)"
  exit 0
fi

{
  echo "loom vs forgejo — $(date -Is)"
  echo "host: $(nproc) cores, $(free -g | awk '/Mem/{print $2}') GB RAM; git/git @ ${SHA:0:12}"
  echo "loom: $(git -C $HOME/Projects/loom log -1 --format='%h %s'); binary $(stat -c %s $(readlink -f /proc/$LPID/exe | sed 's/ (deleted)//') | awk '{printf "%.1f MB", $1/1048576}')"
  echo "forgejo: $(docker exec -u git forgejo-bench forgejo --version | cut -d' ' -f3) sqlite3, defaults"
  echo
  bench loom $LPID $D/loom.tsv $L
  echo
  bench forgejo $FPID $D/forgejo.tsv $F
  echo
  echo "## forgejo container total (all processes incl. s6, sshd)"
  docker stats --no-stream --format '{{.Name}} mem={{.MemUsage}} pids={{.PIDs}}' forgejo-bench
} 2>&1 | tee "$OUT"
echo "saved $OUT"
