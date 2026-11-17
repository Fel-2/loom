# loom vs Forgejo 15.0.6 — results (2026-11-16)

Raw output: `20261116-121815.txt` (full run). Its clone line for Forgejo is
**invalid** (sampler race, fixed in `run.sh`); the corrected clone numbers are
from the re-run below. Host: 20 cores, 15 GB RAM. Forgejo: SQLite, defaults.
loom: commit 574ce40, 13.3 MB binary.

## Memory (the apples-to-apples part)

| | loom | Forgejo | ratio |
|---|---|---|---|
| idle RSS | **16.6 MB** | 239.4 MB | 14× |
| idle threads | **1** | 43 | |
| peak RSS, 32 concurrent clients | **87.5 MB** | 353.5 MB | 4× |
| peak process tree, same load | **87.5 MB** (no children) | 988.6 MB (58 `git` children) | 11× |
| RSS 70 s after load | **16.9 MB** | 304.6 MB | 18× |
| threads after load | **1** | 104 | |
| container total (Forgejo incl. s6, sshd) | — | 303.5 MiB, 108 pids | |

loom's RSS includes ~11 MB of its own mapped binary; private memory is ~6.5 MB.
Forgejo's RSS likewise includes its (much larger) binary.

## Throughput and latency

Fixed work: 16 pages × 40 with 32 concurrent clients.

| | loom | Forgejo |
|---|---|---|
| wall time | **1.13 s** | 12.96 s |
| requests/s | **564** | 49 |
| errors | 0 | 0 |

Uncontended (1 client), p50:

| page | loom | Forgejo | note |
|---|---|---|---|
| issue list | 0.73 ms | 14.6 ms | |
| issue + 3 comments | 1.00 ms | 23.7 ms | |
| user page | 0.30 ms | 4.8 ms | |
| API: repo | 0.26 ms | 1.5 ms | |
| API: issues | 0.29 ms | 10.3 ms | |
| repo home | 1.8 ms | 72 ms | ⚠ Forgejo also shows last commit per entry |
| tree dir | 1.3 ms | 1,039 ms | ⚠ same: last commit per file (expensive) |
| blob `diff.c` | 3.6 ms | 334 ms | ⚠ Forgejo syntax-highlights; loom does too now (see note below) |
| raw file | 1.8 ms | 13.6 ms | identical bytes |
| log | 2.3 ms | 38 ms | |
| log page 50 | 22 ms | 50 ms | |
| commit | 5.6 ms | 63 ms | |
| branches | 18.7 ms | 204 ms | loom's page also lists tags |
| PR | 24.9 ms | 69 ms | |
| PR files | 12.6 ms | 67 ms | |

Pages are also much smaller on loom (e.g. repo home 63 KB vs 787 KB, issue
4.8 KB vs 199 KB).

**Not apples to apples** (⚠): Forgejo computes the last commit touching each
file in tree views; loom does not (it is opt-in per repository). The tree
rows show the cost of that feature as much as the cost of the implementation.
loom **does** highlight source now (added after this table ran, milestone 8):
re-measured 2026-11-17 on the same host, blob `web.rs` (1,425 lines) renders
at 4.2 ms p50 with highlighting on — 20 samples, live server, cold cache
warmed. The issue/API/raw/log rows are like for like.

## Clones (8× ytm-tui + 2× git/git, concurrent, HTTP)

| | loom | Forgejo |
|---|---|---|
| time | **14.5 s** | 16.7 s |
| peak process tree | **1.16 GB** | 1.46 GB |

Both are dominated by `git pack-objects` (same git binary): packing git/git
twice costs ~1.1 GB either way. The difference is the server itself.

## Found in loom by this benchmark

Under 32 concurrent clients, every page that touches git sits at ~80 ms p50
while DB-only pages stay ~1 ms: git work queues behind loom's blocking pool of
**4 threads** (`max_blocking_threads(4)`). That cap is a deliberate memory/latency
trade-off; it's now a tracked issue to tune (e.g. scale with cores, or cache).

## After fixing what the benchmark found (loom only, same data, 4 git threads)

The thread cap wasn't the real problem. Tried 4/8/16/32 threads first:
throughput barely moved (597 → 1034 req/s) while peak RSS grew 81 → 210 MB and
DB-only pages slowed (0.6 → 9.7 ms p50). So instead of more threads, less work
per request:

1. **Repo handles cached per worker thread** (config, refs store, pack indexes
   loaded once, not per request). Freed when idle threads exit after 10 s.
2. **PR summary cached by (base tip, head)**: merge-base, commit list, file
   count and mergeability used to cost ~10 git subprocesses per view.
3. **Single-file history via `git log -- path`** (commit-graph Bloom filters;
   pathspec magic disabled), and commit-graphs written after every push.

| | before | after |
|---|---|---|
| 32-client load, req/s | 564–597 | **1,176** |
| git pages p50 under load | ~80 ms | **~35 ms** |
| PR page, 1 client | 24.9 ms | **1.3 ms** |
| PR files, 1 client | 12.6 ms | **7.1 ms** |
| `log?path=diff.c` | 260 ms | **25 ms** |
| peak RSS under load | 81–88 MB | 185 MB — see below |
| private memory under load | — | **11 MB** |
| RSS 75 s after load | 16.9 MB | **15.3 MB** (3.2 MB private), 1 thread |

Peak RSS rose because cached handles keep git/git's pack files mapped while the
workers are busy: 151 MB of the 185 MB peak is `pack-*.pack` + `.idx` pages —
page cache, shared and reclaimable by the kernel. Private memory under load is
11 MB, and everything is released once the workers idle out.
