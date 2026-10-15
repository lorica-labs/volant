---
title: Tests
description: How Volant is checked against ansible-core, the four kinds of recorded answer, and how to add a scenario.
---

Volant must behave like ansible-core 2.19.12. Most of the test suite is therefore a comparison: ansible-core ran something once, its answer was recorded in the repository, and the test runs Volant on the same input and compares.

## The four kinds

Each kind of recording has a name, and `just golden <kind>` regenerates it.

| Kind | What was recorded | Where it runs |
|---|---|---|
| `template` | The result of rendering each expression in `crates/volant/tests/golden/cases.yml` | Every job, with `reference.rs` |
| `inventory` | Hosts, variables and groups of an inventory, and the hosts each pattern selects | Every job, with `reference.rs` |
| `scenario` | A playbook run: the exit code and the whole standard output | Every job, with `reference.rs` |
| `module` | What each native module answered on a real machine | The `ssh` job and `just test-reference`, with `reference_modules.rs` |

Nothing in a recording is edited by hand. Change the input, run `just golden <kind>` with ansible-core 2.19.12 installed, and read the diff. Running it on an unchanged tree leaves the files exactly as they were. The `module` kind also needs `sudo`, systemd, apt and cron, because it creates an account and a unit on the machine.

## Add a scenario

A scenario is a directory under `crates/volant/tests/scenarios`:

1. Create `crates/volant/tests/scenarios/<name>/` and put in it the playbook, the inventory and any files the playbook reads.
2. Write `case.yml` next to them. A single run is `args: [-i, inv.ini, site.yml]`. Several runs of the same directory are `runs:`, a list of `{args: [...]}`.
3. Run `just golden scenario`. It writes `expected.json` in the directory.
4. Run `touch crates/volant/tests/reference.rs` and then the tests. The list of scenarios is fixed when that file compiles, so a new directory is not seen until it is rebuilt.

Each run of Volant happens in a fresh copy of the directory, with `--forks 1` unless the case says otherwise. The test compares the exit code and the complete standard output, with timings masked. Three more keys are available in `case.yml`:

- `env` sets environment variables for the run.
- `forks` raises the number of forks above one. Hosts may then print in a different order, so list the lines whose order may differ in `unordered`, as regular expressions. `unordered` without `forks` is refused.
- `needs` names what the machine must have: `ansible-core`, `sudo` or `systemd`. A case whose needs are not met is skipped, except when `VOLANT_PYTHON` is set, which turns the skip into a failure.

## Run them

`just test` runs everything that needs no host. `just test-reference` runs the module recordings against the machine you are on, which needs ansible-core, sudo and systemd. `just ssh-test` is what the `ssh` job runs: it starts from an sshd on localhost and covers the end-to-end tests too. Prefer a scenario or a table row in an existing test to a new test function.
