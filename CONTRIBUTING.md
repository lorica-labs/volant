# Contributing to Volant

Thanks for your interest. Bug reports, fixes, features and documentation are all welcome.

## Before you start

- Search existing issues and discussions.
- For anything larger than a fix, open an issue first so we can agree on the approach.
- Security problems: do not open a public issue, see [SECURITY.md](SECURITY.md).

## Setup

Install a recent stable Rust toolchain with [rustup](https://rustup.rs), then:

```sh
cargo install cargo-binstall
cargo binstall just
just setup
```

`just setup` points git at the hooks in `.githooks`, one of which adds the sign-off trailer, and installs the tools used by `just check`. On Linux and macOS, install `actionlint` from your package manager; on Windows, `winget install rhysd.actionlint`. Running the CI workflow locally needs Docker and `gh extension install nektos/gh-act`.

The `ssh_*` tests run a playbook over a real `ssh`, so `just check` leaves them out and `just ssh-test` runs them on their own. That recipe wants an sshd listening on `localhost` and `VOLANT_SSH_TEST_KEY` pointing at a private key this account accepts; a throwaway key appended to your own `authorized_keys` does the job. The tests talk to `localhost` and to nothing else, which is why CI can run them unchanged.

`just bench-compile` times how long Volant and `ansible-playbook` each take to turn a large role into a task list. It clones the `ansible-lockdown/UBUNTU22-CIS` role, which is MIT licensed, into `target/`, and it needs `ansible-playbook` on your PATH. Both sides only list the tasks, so neither connects to a host. Volant has no `--list-tasks` option yet, so the recipe prints the reference timings and then fails on the Volant half.

## Workflow

1. Fork the repository and create a branch from `main`: `feat/…`, `fix/…`, `docs/…`, `ci/…` or `chore/…`.
2. Keep each pull request to one logical change.
3. Run `just check` before pushing. It runs exactly what CI runs.
4. Open a pull request against `main`. Its title must follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/), for example `feat(agent): collect network facts`. The title becomes the commit message when the pull request is squashed.
5. Add a line under `Unreleased` in `CHANGELOG.md` when the change is visible to users.

## Sign your work

Every commit must carry a `Signed-off-by` trailer, which certifies the [Developer Certificate of Origin](https://developercertificate.org). `git commit -s` adds it, and once `just setup` has run, the `prepare-commit-msg` hook adds it for you.

## Code

- Every source file starts with `// SPDX-License-Identifier: GPL-3.0-or-later`.
- `cargo fmt` and `cargo clippy` must pass with no warnings.
- New dependencies need a sentence in the pull request explaining why an existing one does not do the job.
- Tests live next to the code they cover; integration tests live in `tests/`.
