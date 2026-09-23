# Contributing to Volant

Thanks for your interest. Bug reports, fixes, features and documentation are all welcome.

## Before you start

- Search the existing [issues](https://github.com/lorica-labs/volant/issues) and [discussions](https://github.com/lorica-labs/volant/discussions).
- For anything larger than a fix, open an issue first so we can agree on the approach.
- Report security problems privately, as described in [SECURITY.md](SECURITY.md), never in a public issue.
- A playbook that behaves differently under Volant and `ansible-playbook` is a bug. The [compatibility template](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) asks for what we need to reproduce it.

## Set up

Install [rustup](https://rustup.rs). The repository pins its toolchain in `rust-toolchain.toml`, so rustup picks the right version on its own. The minimum supported Rust version is 1.95.

```sh
cargo install cargo-binstall
cargo binstall just
just setup
```

`just setup` points git at the hooks in `.githooks`, one of which adds the sign-off trailer, and installs the tools the other recipes use. You also need `actionlint`: install it from your package manager on Linux and macOS, or with `winget install rhysd.actionlint` on Windows.

## Everyday commands

| Command | What it does |
|---|---|
| `just check` | Formatting, lints and the test suite. Run it before you push. |
| `just test-one <crate> <pattern>` | One crate's tests matching a pattern, for the edit-and-retry loop. |
| `just coverage` | Line coverage, gated at the floor CI enforces. |
| `just ssh-test` | The tests that run a playbook over a real `ssh`, see below. |
| `just mutants [file]` | Mutation testing on one file. |
| `just bench-compile` | Times Volant against `ansible-playbook` on a large real role, see below. |
| `just docs-keywords`, `just docs-modules` | Regenerate the keyword and module reference pages from the source tables. |
| `just ci-local` | Runs the CI workflow locally. Needs Docker and `gh extension install nektos/gh-act`. |

CI runs more than `just check`: the SSH tests, the coverage floor, a musl build, a build on the minimum supported Rust version, `cargo publish --dry-run`, and the tests again on macOS.

### SSH tests

The `ssh_*` tests run a playbook over a real `ssh`, so `just check` leaves them out. `just ssh-test` needs three things:

1. An sshd listening on `localhost`.
2. `VOLANT_SSH_TEST_KEY` pointing at a private key your account accepts. A throwaway key added to your own `~/.ssh/authorized_keys` works.
3. `VOLANT_PYTHON` naming a Python that has `ansible-core` installed, in a virtual environment:

   ```sh
   python3 -m venv .venv
   .venv/bin/pip install ansible-core==2.19.12
   export VOLANT_PYTHON=$PWD/.venv/bin/python
   ```

   It must not be the system `python3`. The tests use it as the controller's Python, and the point of the check is that the controller and the managed host use two different interpreters.

The tests only talk to `localhost`, which is why CI can run them unchanged.

### The compile benchmark

`just bench-compile` clones the MIT-licensed `ansible-lockdown/UBUNTU22-CIS` role into `target/`, at a commit pinned in the recipe. It needs `ansible-playbook` on your `PATH`. Both engines only list the tasks, so nothing connects to a host. The recipe alternates three runs of each engine and compares the two listings after every round. It prints `listings identical` and exits 0 when all three rounds agree, or fails with `listings differ` on the first round that does not.

### The golden corpus

`crates/volant/tests/golden` holds what ansible-core 2.19.12 produced for a set of templates, patterns, listings and Python modules. The tests compare Volant against it. `generate.py` in that directory regenerates it, and needs ansible-core 2.19.12 installed. Output snapshots under `crates/volant/tests/snapshots` are reviewed with `cargo insta review`.

## Documentation

The documentation site lives in `docs/` and is built with [Starlight](https://starlight.astro.build). Pages are Markdown or MDX files under `docs/src/content/docs/`. To preview it:

```sh
just docs-dev
```

A behavior change comes with its documentation in the same pull request. Two pages are generated from the source, `reference/keywords.md` and `reference/modules.md`: edit the tables in the code and run `just docs-keywords` or `just docs-modules`, never the page itself.

Write in American English, one paragraph per line, with sentence-case headings. `just docs-build` checks every internal link.

The install script is `docs/public/install.sh`, served from the site root. It is POSIX `sh` and must pass `shellcheck -s sh`.

Diagrams are Mermaid code blocks. A tutorial video goes in an MDX page with the `Video` component from `docs/src/components/Video.astro`, either from YouTube (`youtube="<id>"`) or as a file under `docs/public/videos/` (`src="/videos/<name>.mp4"`).

## Workflow

1. Fork the repository and create a branch from `main`: `feat/…`, `fix/…`, `docs/…`, `ci/…` or `chore/…`.
2. Keep each pull request to one logical change.
3. Run `just check` before pushing.
4. Open a pull request against `main`. Its title must follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/), for example `feat(agent): collect network facts`. The title becomes the commit message when the pull request is squashed.
5. When the change is visible to users, add a line under `Unreleased` in `CHANGELOG.md`, written for users.

## Sign your work

Every commit needs a `Signed-off-by` trailer, which certifies the [Developer Certificate of Origin](https://developercertificate.org). `git commit -s` adds it, and after `just setup` the `prepare-commit-msg` hook adds it for you.

## Code

- Every source file starts with `// SPDX-License-Identifier: GPL-3.0-or-later`.
- `cargo fmt` and `cargo clippy` must pass with no warnings.
- A new dependency needs a sentence in the pull request explaining why an existing one does not do the job.
- Unit tests live next to the code they cover, integration tests in `tests/`.
- [ARCHITECTURE.md](ARCHITECTURE.md) maps the source tree.
