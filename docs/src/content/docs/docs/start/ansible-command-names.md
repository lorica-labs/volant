---
title: Ansible command names
description: Which Volant command replaces each ansible-core program, what is the same, what differs, and how to keep your command lines.
---

Volant runs the nine programs of ansible-core 2.19.12. Each one is a `volant` subcommand, and on Linux and macOS it can also be started under its ansible name. Windows is not supported for the program names or for the links.

## What replaces what

| ansible-core program | Volant command | Runs in | Managed runtime |
|---|---|---|---|
| `ansible` | [`volant adhoc`](/docs/reference/cli/adhoc/) | Volant | not needed for the native modules |
| `ansible-playbook` | [`volant playbook`](/docs/reference/cli/), `volant-playbook` | Volant | installed on first use that needs Python |
| `ansible-vault` | [`volant vault`](/docs/reference/cli/vault/) | Volant | not needed |
| `ansible-inventory` | [`volant inventory`](/docs/reference/cli/inventory/) | Volant | not needed, except for inventory plugins of collections |
| `ansible-doc` | [`volant doc`](/docs/reference/cli/doc/) | ansible-core's own tool | installed on first use |
| `ansible-config` | [`volant config`](/docs/reference/cli/config/) | ansible-core's own tool | installed on first use |
| `ansible-galaxy` | [`volant galaxy`](/docs/reference/cli/galaxy/) | ansible-core's own tool | installed on first use |
| `ansible-pull` | [`volant pull`](/docs/reference/cli/pull/) | Volant | not needed for a `git` checkout |
| `ansible-console` | [`volant console`](/docs/reference/cli/console/) | Volant | not needed for the native modules |

The managed runtime installs itself the first time a command needs it, as it does for a playbook, and prints one line on standard error when it does. [The managed runtime](/docs/reference/runtime/) describes it. You do not need to install ansible, but three of the nine commands run ansible-core's own code from that runtime.

## Keeping your command lines

The links are not made unless you ask. Either run the install script with `--shims`, or run [`volant shims install`](/docs/reference/cli/shims/):

```sh
curl -fsSL https://volant.sh/install.sh | sh -s -- --shims
volant shims install
```

Each name is a symbolic link to `volant`. Volant reads the file name it was started as, so the link `ansible-vault` runs `volant vault`. Both commands take the same arguments and give the same output and exit codes. A usage error exits with 2 and names the program as started, `ansible-vault: error: ...`.

`install` does not replace a file or link it did not make, unless you give `--force`. When another program of that name is on your `PATH`, it says so: `note: /usr/bin/ansible-vault is hidden by this link, DIR comes first in PATH` when the link wins, or `note: ... still runs first, DIR comes later in PATH` when it does not. To remove the links:

```sh
volant shims remove
```

`remove` deletes only links that point at a file called `volant`, and leaves everything else.

A CI script like this one runs unchanged once the links exist, with no ansible on the machine:

```sh
ansible-inventory -i hosts.ini --list > inventory.json
ansible-vault view group_vars/all/secrets.yml --vault-password-file ci-pass
ansible-playbook -i hosts.ini site.yml
```

## What is the same

For `ansible`, `ansible-vault` and `ansible-inventory`, the output, the options listed on the command pages and the exit codes were measured against ansible-core 2.19.12. Each claim has a test behind it.

| Claim | Where it is checked |
|---|---|
| `volant adhoc` prints the `minimal` callback's bytes, the `[ERROR]` block and the `-o` and `-t` forms, with the reference's exit codes | the `adhoc-*` scenario goldens and `crates/volant/tests/adhoc_cli.rs` |
| The `ansible` name runs `volant adhoc` | `the_ansible_name_runs_adhoc` |
| The `ansible-playbook` name runs `volant playbook` | `a_link_named_ansible_playbook_behaves_as_the_playbook_subcommand` |
| `volant vault` gives ansible-vault's messages, exit codes, file modes and files | `crates/volant/tests/vault_cli.rs`, and the `vault-*` scenario goldens for the vault data a playbook reads |
| A file one tool encrypts, the other decrypts, for the default id and for a label | `a_file_one_tool_encrypts_the_other_decrypts` |
| `volant inventory` prints ansible-inventory's JSON and YAML bytes: key order, four-space indent, PyYAML anchors | `crates/volant/tests/inventory_golden.rs` over `golden/inventories/cases.yml`, and `inventory_cli.rs` |
| `volant config` and `volant galaxy` print what the tool prints | `crates/volant/tests/tools_cli.rs` |
| `volant pull` prints the playbook run and the `-o` line as `ansible-pull` does, and checks out without Python | `a_pull_prints_what_the_reference_pull_prints` and `a_pull_checks_out_and_runs_without_python` in `console_pull.rs` |
| `volant console` prints what `ansible-console` prints for its commands, modules and shell lines on a pipe, and needs no Python for native modules | `the_console_reads_its_commands_from_a_pipe` and `the_console_runs_without_python` in `console_pull.rs` |
| The links `install` makes, the lines it prints and what `remove` leaves alone | `crates/volant/tests/shims.rs` |
| Which names map to which command, and no other name does | the tests in `crates/volant/src/cli/program.rs` |

The options each native command accepts are listed on its own page: [`volant adhoc`](/docs/reference/cli/adhoc/), [`volant vault`](/docs/reference/cli/vault/), [`volant inventory`](/docs/reference/cli/inventory/) and [`volant pull`](/docs/reference/cli/pull/). An option that is not listed there is refused, never ignored.

## What differs

- The help text is Volant's. It is never compared with the reference.
- `--version` prints `ansible [core 2.19.12]` (or the program's name) and the paths. The Python, Jinja and PyYAML lines are left out when Volant finds no interpreter with ansible-core, because `--version` installs nothing. See [`volant adhoc`](/docs/reference/cli/adhoc/).
- `executable location` in `--version` is the program as it was started: `volant`, or the link. See [`volant doc`](/docs/reference/cli/doc/#terminal-exit-code-and---version).
- `ansible-inventory --toml` needs no Python package, where the reference needs `tomli-w`, and a `null` value gives one `[ERROR]` line instead of a traceback (both exit 250). See [`volant inventory`](/docs/reference/cli/inventory/).
- `volant pull` prints its first two lines before what its steps print, and its checkout starts no Python. See [`volant pull`](/docs/reference/cli/pull/#differences-from-ansible-pull).
- `volant config dump` shows `COLLECTIONS_PATHS` as set by the environment once the runtime has a `collections/` directory. See [`volant config`](/docs/reference/cli/config/#the-collections-path).
- `ansible-vault view` goes through a pager only on a terminal, with `PAGER`, then `ANSIBLE_PAGER`, then `[defaults] pager`, then `less`. See [Vault](/docs/variables/vault/).
- `volant adhoc` and `volant playbook` use Volant's engine, so the differences listed in [Will my playbook run?](/docs/start/compatibility/) and on [`volant adhoc`](/docs/reference/cli/adhoc/#actions-that-are-not-modules) apply.

## Exit codes

| Command | 0 | 1 | 2 | 4 | 5 | 250 |
|---|---|---|---|---|---|---|
| `volant adhoc` | every host succeeded or was skipped | no host left to target, or a bad `meta` action | a host failed, or a usage error | a host was unreachable, or something this release does not support yet | an invalid setting, or no argument for `command` | an argument that is not valid JSON |
| `volant vault` | done | a file error, such as `create` on an existing file | a usage error | | a bad option, such as two files with `--output` | |
| `volant inventory` | printed | | a usage error | | no action or two actions, or a bad `--host` or `--graph` target | |

`volant playbook` uses the codes on [Exit codes](/docs/reference/exit-codes/). The three tools that run in ansible-core's runtime return the tool's own code, or 1 with an `[ERROR]:` line when the interpreter cannot be started. `volant pull` exits 2 when the checkout fails, 5 for an options error or when no playbook is found, and otherwise with the playbook's code.
