---
title: Tests
description: How Volant is checked against ansible-core, the kinds of recorded answer, and how to add a scenario.
---

Volant must behave like ansible-core 2.19.12. Most of the test suite is therefore a comparison: ansible-core ran something once, its answer was recorded in the repository, and the test runs Volant on the same input and compares.

## The kinds

Each kind of recording has a name, and `just golden <kind>` regenerates it.

| Kind | What was recorded | Where it runs |
|---|---|---|
| `template` | The result of rendering each expression in `crates/volant/tests/golden/cases.yml` | Every job, with `reference.rs` |
| `inventory` | Hosts, variables and groups of an inventory, and the hosts each pattern selects | Every job, with `reference.rs` |
| `scenario` | A playbook run: the exit code and the whole standard output | Every job, with `reference.rs` |
| `command-names` | The output of the scripts in `tests/command-names`, which call `ansible`, `ansible-playbook`, `ansible-vault`, `ansible-inventory`, `ansible-doc`, `ansible-config`, `ansible-galaxy`, `ansible-pull` and `ansible-console` | The `command-names` job |
| `shadow` | A run of a chosen scenario through the `volant.compat.record` callback, gzipped in `crates/volant/tests/golden/shadow`; the callback's own sample under every ansible-core it supports is checked too (`just compat-matrix`) | Every job, with `shadow_golden.rs`, which replays each recording and wants no divergence |
| `module` | What each native module answered on a real machine | The `ssh` job and `just test-reference`, with `reference_modules.rs` |
| `galaxy` | An `ansible-galaxy` command run against a fake Galaxy server (`golden/reference/galaxy_server.py`): the exit code, standard output and error, the requests the server saw, and the files left behind with their SHA-256, in `crates/volant/tests/golden/galaxy/<case>/expected.json` | Every job, with `galaxy_golden.rs`, which needs Python 3 for the server |

Nothing in a recording is edited by hand. Change the input, run `just golden <kind>` with ansible-core 2.19.12 installed, and read the diff. Running it on an unchanged tree leaves the files exactly as they were. The `module` kind also needs `sudo`, systemd, apt and cron, because it creates an account and a unit on the machine.

A `galaxy` case is a `case.yml` holding `args`, the arguments after `ansible-galaxy`, where `<golden-server>` stands for the server's URL and `<golden-tmp>` for the scratch directory of the run. The server builds its collections and roles itself, with fixed times and owners, so an archive has the same SHA-256 everywhere. In a recording the scratch directory reads `<golden-tmp>` and the port `<golden-port>`. Every other file in the case's directory is copied into the scratch directory, where the command runs, so `-r requirements.yml` can name one. `galaxy_golden.rs` replays each recorded request against the server, and runs `volant galaxy` on every case whose subcommand is native.

## The command-names job

The `command-names` job runs on a `debian:stable-slim` container with no `ansible` and no Python. It links the `ansible-*` names to the static `volant` with `volant shims install`, then runs the scripts of `tests/command-names` exactly as a user would write them for ansible. Each script writes its output to a file and compares it with `tests/command-names/expected/<script>.out`, a recording of ansible-core 2.19.12 made by `just golden command-names`. The job uploads the output files when a script fails.

## Add a scenario

A scenario is a directory under `crates/volant/tests/scenarios`:

1. Create `crates/volant/tests/scenarios/<name>/` and put in it the playbook, the inventory and any files the playbook reads.
2. Write `case.yml` next to them. A single run is `args: [-i, inv.ini, site.yml]`. Several runs of the same directory are `runs:`, a list of `{args: [...]}`.
3. Run `just golden scenario`. It writes `expected.json` in the directory.
4. Run `touch crates/volant/tests/reference.rs` and then the tests. The list of scenarios is fixed when that file compiles, so a new directory is not seen until it is rebuilt.

Each run of Volant happens in a fresh copy of the directory, with `--forks 1` unless the case says otherwise. The test compares the exit code and the complete standard output, with timings masked. Besides `args` and `runs`, `case.yml` takes six more keys:

- `modes` lists `check`, `check-diff` and `diff`. Each run then happens once more per mode, with `--check`, `--check --diff` or `--diff` added, in that fixed order, and every run is recorded.
- `env` sets environment variables for the run.
- `program: ansible` records the runs with `ansible` instead of `ansible-playbook`, and runs `volant adhoc` instead of `volant playbook`.
- `forks` raises the number of forks above one. Hosts may then print in a different order, so list the lines whose order may differ in `unordered`, as regular expressions. `unordered` without `forks` is refused.
- `needs` names what the machine must have: `ansible-core`, `sudo` or `systemd`. A case whose needs are not met is skipped, except when `VOLANT_PYTHON` or `VOLANT_REQUIRE_REFERENCE=1` is set, which turns the skip into a failure: nextest does not show the skip of a test that passes, so a machine without ansible-core would report green. The `ssh-test` recipes set `VOLANT_REQUIRE_REFERENCE=1`.
- `links` maps a path to the target of a link that each copy gets at that path. Use it for a link that git or cargo cannot hold, such as a link to a file that does not exist: `cargo package` refuses a tracked link like that.

## The parity matrix

`crates/volant/tests/parity.rs` compares every name that ansible-core 2.19.12 and the collections of the pinned `ansible` package know (keywords, modules, filters, tests, lookups and the other plugin kinds) with what the engine does with it. `just parity-reference` regenerates the reference side, `crates/volant/tests/parity/reference.json`. The [Parity](/docs/reference/parity/), [Keywords](/docs/reference/keywords/) and [Modules](/docs/reference/modules/) pages are generated from the engine's own tables by `just docs-parity`, `just docs-keywords` and `just docs-modules`, and a test fails when one of them is out of date.

## The YAML corpus

`just yaml-corpus` loads every YAML file of a pinned corpus with Volant's loader and with ansible-core's, and compares the values. The corpus is the collections of the pinned `ansible` package, ansible-core's own `test/` tree, and public repositories at the commits `crates/volant/tests/parity/yaml_corpus.yml` names: kubespray, debops, ceph-ansible, openstack-ansible and others, about eleven thousand files. The example `yaml_dump` prints Volant's side, with Volant's own encodings undone, and `crates/volant/tests/parity/yaml_corpus.py` prints the files that differ, grouped by cause. A date is read as text by Volant, and that difference is not counted.

`crates/volant/tests/parity/yaml_corpus_known.txt` lists each known difference with the issue that tracks it. The recipe fails on any other one. The `YAML corpus` workflow runs it every week and on every change to the loader. Remove an entry when its issue is fixed.

## Run them

`just check-touched` is the check to run before a pull request. It formats, runs clippy and the file-size ratchet, runs the tests of the modules the branch changes, and checks spelling. `just check-local` does the same with every test. CI runs every test.

`just test` runs everything that needs no host. `just test-reference` runs the module recordings against the machine you are on, which needs ansible-core, sudo and systemd. `just ssh-test` is what the `ssh` job runs: it starts from an sshd on localhost and covers the end-to-end tests too. Prefer a scenario or a table row in an existing test to a new test function.
