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
| blob `diff.c` | 3.6 ms | 334 ms | ⚠ Forgejo syntax-highlights; loom doesn't yet |
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
file in tree views and highlights source; loom does neither yet. Those rows show
the cost of features loom lacks as much as the cost of the implementation. The
issue/API/raw/log rows are like for like.

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
