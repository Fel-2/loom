# Contributing

Small, focused changes are welcome. Read the scope first — it saves both of us
a round trip.

## Scope

loom deliberately stays small. These will be closed without discussion:

- Federation, ActivityPub, shared namespaces
- A built-in CI runner, package registry, wiki, or project boards
- Any external service (Redis, Postgres, an object store, Node at runtime)
- Background worker pools; the design is "no workers"

Running integrations belong outside: webhooks plus the commit-status API are
there so Jenkins, Forgejo Actions, or a ten-line script can do the job.

## Build and test

```sh
cargo build --release
cargo test --release        # unit tests: TOTP, HMAC, tokenizer, highlighter
```

The shell suites run against a live instance. The easiest way to get one is the
bundle, which seeds users, keys and fixture repositories:

```sh
./scripts/make-test-bundle.sh
cp -a dist/loom-project-0.1.0/test-data data-run
LOOM_DATA=$PWD/data-run LOOM_ADDR=127.0.0.1:3917 LOOM_SSH_ADDR=127.0.0.1:2917 \
  LOOM_URL=http://127.0.0.1:3917 LOOM_SSH_HOST=127.0.0.1:2917 \
  dist/loom-project-0.1.0/loom serve &

export LOOM=$PWD/dist/loom-project-0.1.0/loom
export LOOM_DATA=$PWD/data-run
export LOOM_TEST_URL=http://127.0.0.1:3917
export LOOM_TEST_SSH_PORT=2917
export KEYS=$PWD/dist/loom-project-0.1.0/test-keys
for suite in smoke m3 m4 m5 m6 m7 m8 m9 m10; do ./tests/$suite.sh; done
```

CI runs exactly that on every push. `m6` needs `python3` (it ships its own
webhook receiver in `tests/hookrecv/`), `m8` wants `git-lfs`, `m10` uses
`sqlite3`.

The suites create and delete repositories, so point them at a throwaway
`LOOM_DATA`, never at a forge you care about.

## Style

- Match the surrounding code. The tree is not `rustfmt`-clean, so do not run
  `cargo fmt` over files you are not changing — it buries the actual diff.
- Comments are sparse by design: the code says what, comments only say why when
  the why is not obvious. Do not annotate every block.
- Server-rendered HTML only. No JavaScript on a page that has to work, and no
  build step for CSS.
- New behaviour needs a test in the shell suite that covers it, not just a unit
  test. The suites are the contract.

## Commits and pull requests

History uses a prefix: `feat:`, `fix:`, `test:`, `docs:`, `deploy:`, `build:`,
`perf:`. Keep the subject under ~70 characters and explain the failure mode in
the body — what went wrong, why this fixes it.

A pull request should say what you broke, not just what you built: the scenario
that used to fail and now does not. CI must be green.

## Security

Do **not** open a public issue for anything exploitable. See
[SECURITY.md](SECURITY.md) for the threat model and how to report privately.

## Licence

Contributions are accepted under GPL-3.0-or-later, the same as the project.
