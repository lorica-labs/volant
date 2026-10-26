# Contributing to Volant

Thanks for your interest. Bug reports, fixes, features and documentation are all welcome.

## Before you start

- Search the existing [issues](https://github.com/lorica-labs/volant/issues) and [discussions](https://github.com/lorica-labs/volant/discussions).
- Looking for something to work on? The [roadmap board](https://github.com/orgs/lorica-labs/projects/1) groups the planned work by release. Each issue gives the expected behaviour, the acceptance criteria and where to start; comment on it before you begin.
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
| `just container-test <runtime>...` | The tests that run a playbook over a real container connection, see below. |
| `just smoke-embedded` | Builds a controller with its agents embedded, as a release does, and runs one task with no agent beside it. |
| `just link-check <archive \| target=file>...` | Checks that release binaries link what they promise: a static musl binary, a glibc binary that needs no glibc above 2.17, a macOS binary with system libraries only. |
| `just scratch-test <controller>` | Runs a static controller in a `FROM scratch` container, as the release gate does. Needs Docker and a static busybox. |
| `just mutants [file]` | Mutation testing on one file. |
| `just bench-compile` | Times Volant against `ansible-playbook` on a large real role, see below. |
| `just docs-keywords`, `just docs-modules` | Regenerate the keyword and module reference pages from the source tables. |
| `just ci-local` | Runs the CI workflow locally. Needs Docker and `gh extension install nektos/gh-act`. |

CI runs more than `just check`: the SSH tests, the container connection tests, the coverage floor, a musl build, the link checks of the static and glibc controllers, the controller in a `FROM scratch` container, a build on the minimum supported Rust version, `cargo publish --dry-run`, and the tests again on macOS.

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

The test of the `ansible_password` variable also needs an account on that sshd that logs in with a password. Name it in `VOLANT_SSH_TEST_PASSWORD_USER` and give its password in `VOLANT_SSH_TEST_PASSWORD`. Without them the test says so and passes.

The tests only talk to `localhost`, which is why CI can run them unchanged.

### Container connection tests

The `<runtime>_*` tests run a playbook through a real container runtime, so `just check` leaves them out. `just container-test <runtime>...` runs the tests of each runtime named, one after the other. It builds an unoptimised musl agent first and stops with a message when the runtime's program is not on your `PATH`. Every runtime also needs `VOLANT_PYTHON`, set up as for the SSH tests, and the plugin's collection on `ANSIBLE_COLLECTIONS_PATH`:

| Runtime | Program | Collection | What else |
|---|---|---|---|
| `docker` | `docker`, usable without `sudo` | community.docker | |
| `podman` | `podman`, rootless works | containers.podman | |
| `buildah` | `buildah`, rootless works | containers.podman | |
| `chroot` | `chroot` | community.general | `VOLANT_CHROOT_TEST_TREE` naming a root filesystem, such as an unpacked Alpine image. Its `/bin/sh` is checked on your machine, as ansible-core checks it, so Alpine's link to `/bin/busybox` needs a busybox there too. The tests run as root, through `sudo` when you are not. |
| `kubectl` | `kubectl` reaching a cluster | kubernetes.core | A cluster such as kind or k3s, through `KUBECONFIG`. The ten-minute idle-link test runs only by hand. |
| `lxd` | `lxc` talking to an initialised LXD | community.general | |

Each test file names the variables that pick its images, such as `VOLANT_DOCKER_TEST_IMAGE`. CI pins them by digest.

Two connections have no CI job: Incus, because the runner has no package for it, and `aws_ssm`, because it needs an AWS account. To run the Incus test against your own server, run `just agent-musl`, then `VOLANT_AGENT_DIR=target/agents cargo nextest run -p volant --test lxd_e2e --run-ignored ignored-only -E 'test(/^incus_/)'`.

### The compile benchmark

`just bench-compile` clones the MIT-licensed `ansible-lockdown/UBUNTU22-CIS` role into `target/`, at a commit pinned in the recipe. It needs `ansible-playbook` on your `PATH`. Both engines only list the tasks, so nothing connects to a host. The recipe alternates three runs of each engine and compares the two listings after every round. It prints `listings identical` and exits 0 when all three rounds agree, or fails with `listings differ` on the first round that does not.

### The golden corpus

`crates/volant/tests/golden` holds what ansible-core 2.19.12 produced for a set of templates, inventory patterns and modules, and each directory of `crates/volant/tests/scenarios` holds a playbook with the output it printed. The tests compare Volant against both. `just golden` regenerates them all, or only the kinds you name (`template`, `inventory`, `scenario`, `command-names`, `shadow`, `module`), and needs ansible-core 2.19.12 installed; `module` also needs `sudo`, systemd, apt and cron, and `shadow` needs `uv` for the callback's check under each ansible-core. Output snapshots under `crates/volant/tests/snapshots` are reviewed with `cargo insta review`.

Some template rows import a Python package beyond ansible-core, named in the row's `needs` list. `crates/volant/tests/golden/requirements.txt` lists them all, with `jmespath` and `netaddr`, at the versions the recordings were made with. CI installs that file. Record the goldens in an environment that has it too:

```sh
uv pip install --python "$(uv tool dir)/ansible-core/bin/python" -r crates/volant/tests/golden/requirements.txt
```

The replay skips a row, and prints its template and the missing package, when the interpreter it runs under lacks one. A row may only name a package that file lists.

## Documentation

The site lives in `docs/` and is built with [Astro](https://astro.build): the project pages in `docs/src/pages/`, the documentation with [Starlight](https://starlight.astro.build). Documentation pages are Markdown or MDX files under `docs/src/content/docs/docs/`, served under `/docs/`. To preview it:

```sh
just docs-dev
```

A behavior change comes with its documentation in the same pull request. Three pages are generated from the source, `reference/keywords.md`, `reference/modules.md` and `reference/parity.md`: edit the tables in the code and run `just docs-keywords`, `just docs-modules` or `just docs-parity`, never the page itself.

Write in American English, one paragraph per line, with sentence-case headings. `just docs-build` checks every internal link.

English is the source. The site and the documentation are also in French, German, Spanish and Simplified Chinese: the documentation under `docs/src/content/docs/<lang>/`, the site's strings in `docs/src/i18n/ui/<lang>.ts`, the sidebar labels in `docs/src/i18n/sidebar.ts`. You can change an English page without touching its translations. Each translated page records which English text it came from, and once the English page changes, the translation shows a notice that it may be out of date. `just docs-i18n-check` lists those pages. After you bring a translation up to date, run `just docs-i18n-stamp`. The generated pages stay in English in every language.

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
