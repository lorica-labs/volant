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

The `ssh_*` tests run a playbook over a real `ssh`, so `just check` leaves them out and `just ssh-test` runs them on their own. That recipe wants an sshd listening on `localhost`, `VOLANT_SSH_TEST_KEY` pointing at a private key this account accepts, and `VOLANT_PYTHON` naming a python that has `ansible-core` installed. A throwaway key appended to your own `authorized_keys` covers the first; a virtualenv covers the second: `python3 -m venv .venv && .venv/bin/pip install ansible-core==2.19.12 && export VOLANT_PYTHON=$PWD/.venv/bin/python`. `VOLANT_PYTHON` must not point at the system `python3`: the tests use it to play the controller, and the whole point of the check in CI is that the controller and the managed host's interpreter are two different pythons. The tests talk to `localhost` and to nothing else, which is why CI can run them unchanged.

`just bench-compile` times how long Volant and `ansible-playbook` each take to turn a large role into a task list. It clones the `ansible-lockdown/UBUNTU22-CIS` role, which is MIT licensed, into `target/` at a commit pinned in the recipe, and it needs `ansible-playbook` on your PATH. Both sides only list the tasks, so neither connects to a host. The recipe alternates three runs of each engine, diffing the two listings after every round: it prints `listings identical` and exits 0 when all three agree, or fails with `listings differ` on the first round that does not.

## Workflow

1. Fork the repository and create a branch from `main`: `feat/…`, `fix/…`, `docs/…`, `ci/…` or `chore/…`.
2. Keep each pull request to one logical change.
3. Run `just check` before pushing. It covers formatting, lints and the test suite. CI reports more than that: the `ssh_*` tests, the coverage floor, a musl build, a build on the minimum supported Rust version, `cargo publish --dry-run`, and the tests again on macOS. Of those, `just ssh-test` and `just coverage` run here. `just check` is the gate to run before pushing, not the loop while iterating: `just test-one <crate> <pattern>` runs one crate's tests matching a pattern, and `just mutants [file]` runs three jobs in parallel (disk-bound on this repository's `target/`, not CPU-bound).
4. Open a pull request against `main`. Its title must follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/), for example `feat(agent): collect network facts`. The title becomes the commit message when the pull request is squashed.
5. Add a line under `Unreleased` in `CHANGELOG.md` when the change is visible to users.

## Sign your work

Every commit must carry a `Signed-off-by` trailer, which certifies the [Developer Certificate of Origin](https://developercertificate.org). `git commit -s` adds it, and once `just setup` has run, the `prepare-commit-msg` hook adds it for you.

## Code

- Every source file starts with `// SPDX-License-Identifier: GPL-3.0-or-later`.
- `cargo fmt` and `cargo clippy` must pass with no warnings.
- New dependencies need a sentence in the pull request explaining why an existing one does not do the job.
- Tests live next to the code they cover; integration tests live in `tests/`.
