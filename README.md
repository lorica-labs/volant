<p align="center">
  <img src="https://raw.githubusercontent.com/lorica-labs/volant/main/docs/assets/volant-mark.png" width="88" height="88" alt="">
</p>

# Volant

Fast engine for Ansible playbooks.

Volant reads the playbooks, roles and inventories you already have, in Ansible's own file formats and under its variable precedence and templating. It executes less than it reads, and what it cannot execute it refuses by name. It drives Linux hosts from Linux or macOS, uploads a small static agent once per host and keeps it for the whole run, so no task starts a Python interpreter. Collections, Python modules in a warm interpreter and a `plan` command that shows what would change are what the project is building towards.

**Status: pre-alpha.** A play compiles and runs end to end: `pre_tasks`, roles with their dependencies and argument specs, tasks and `post_tasks`; blocks with `rescue` and `always`; handlers, with `listen`, `meta: flush_handlers` and `--force-handlers`; tags and the four listing commands; `serial` batches; `until` retries; `no_log`; `environment`; `run_once` and `delegate_to`; and `include_tasks`, `include_role` and `include_vars` read while the play runs. Tasks travel over SSH with the agent cached on each host, with `become` through `sudo`, `forks` and the full host-pattern grammar.

Seven [modules](docs/src/modules.md) run and no others: `command`, `shell` and `raw` on the host, `debug`, `set_fact`, `include_vars` and `validate_argument_spec` on the controller. Collections, fact gathering and check mode are not there yet. A playbook that names a module outside those seven is refused by that name before the first connection rather than half-run. One gap in that promise: the check reads the play as it was compiled, so the pre-flight never sees a file that only a dynamic `include_tasks` or `include_role` names. A module met that way fails during the run, on a host that is already connected. A keyword this release would have refused is worse there, because it is accepted and then ignored instead, so the run can report success without having done what the playbook asked, and `check_mode: true` runs the task for real. What `import_tasks` and `import_role` name is compiled with the play and checked with it. [Keywords](docs/src/keywords.md) lists what this release executes, what it only partly answers and what it refuses.

## Installation

Every release on the [releases page](https://github.com/lorica-labs/volant/releases) carries one archive per platform. An archive holds the controller `volant`, its `volant-playbook` alias, the agent for the machine you run on, and the two Linux musl agents the controller uploads to the hosts it manages. One download covers a first run:

```sh
tag=TAG   # the tag you picked from the releases page
curl -fsSL "https://github.com/lorica-labs/volant/releases/download/$tag/volant-x86_64-unknown-linux-musl.tar.xz" | tar -xJ
cd volant-x86_64-unknown-linux-musl
```

The controller looks for its agents in the directory its own executable sits in. Keep them together: move the directory as a whole, and link to `volant` from somewhere on your `PATH` rather than copying it out.

### A first run, here

```sh
printf 'localhost ansible_connection=local\n' > inventory.ini
printf -- '- hosts: localhost\n  gather_facts: false\n  tasks:\n    - command: id -un\n' > site.yml
./volant playbook -i inventory.ini site.yml
```

```text
PLAY [localhost] ***************************************************************

TASK [command] *****************************************************************
changed: [localhost]

PLAY RECAP *********************************************************************
localhost                  : ok=1    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=0
```

### A first run over ssh

The host needs an sshd and an account you can log into. It needs no Python and nothing installed by hand: the controller uploads the agent with the first task and caches it there for the next run. What you need on this side is the key:

```sh
# the address, the account and the key are yours to fill in
printf 'web1 ansible_host=192.0.2.10 ansible_user=deploy ansible_ssh_private_key_file=/home/you/.ssh/id_ed25519\n' > inventory.ini
printf -- '- hosts: web1\n  gather_facts: false\n  tasks:\n    - command: id -un\n' > site.yml
./volant playbook -i inventory.ini site.yml
```

### From source

```sh
cargo install --git https://github.com/lorica-labs/volant volant
```

This installs the controller alone, and so do `cargo binstall volant` and the shell installer: all three copy `volant` and `volant-playbook` and leave the agents behind. A controller with no agent beside it fails on its first task. Point `VOLANT_AGENT_DIR` at a directory that holds `volant-agent` and `volant-agent-<target triple>`, for instance an unpacked release archive, or work from a checkout, where `cargo build` leaves the agent next to the controller it just built.

## Promises

- Your files stay yours. Ansible's own formats, its variable precedence and its templating, with no conversion step and no new format to learn. What this release cannot execute yet it refuses by name, as the paragraphs above set out.
- Linux targets over SSH first. Windows and network devices come later.
- Volant collects no telemetry and makes no network call other than what your playbook asks for.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Questions go to [Discussions](https://github.com/lorica-labs/volant/discussions), bugs and feature requests to [Issues](https://github.com/lorica-labs/volant/issues).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
