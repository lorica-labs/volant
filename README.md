<p align="center">
  <img src="https://raw.githubusercontent.com/lorica-labs/volant/main/docs/assets/volant-mark.png" width="96" height="96" alt="">
</p>

<h1 align="center">Volant</h1>

<p align="center">
  <strong>Your Ansible playbooks, run by a fast engine written in Rust.</strong>
</p>

<p align="center">
  <a href="https://github.com/lorica-labs/volant/actions/workflows/ci.yml"><img src="https://github.com/lorica-labs/volant/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://codecov.io/gh/lorica-labs/volant"><img src="https://codecov.io/gh/lorica-labs/volant/graph/badge.svg" alt="Coverage"></a>
  <a href="https://crates.io/crates/volant"><img src="https://img.shields.io/crates/v/volant?include_prereleases&label=crates.io" alt="crates.io"></a>
  <a href="https://github.com/lorica-labs/volant/releases"><img src="https://img.shields.io/github/v/release/lorica-labs/volant?include_prereleases&label=release" alt="Latest release"></a>
  <a href="https://volant.sh/"><img src="https://img.shields.io/badge/docs-volant.sh-0d9488" alt="Documentation"></a>
  <a href="https://github.com/lorica-labs/volant/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="License"></a>
</p>

<p align="center">
  <a href="https://volant.sh/">Documentation</a> ·
  <a href="https://volant.sh/start/installation/">Installation</a> ·
  <a href="https://volant.sh/start/quickstart/">Quickstart</a> ·
  <a href="https://volant.sh/start/compatibility/">Will my playbook run?</a>
</p>

<!-- A short demo video goes here once it is recorded. -->

Volant reads the playbooks, roles and inventories you already have, in Ansible's own formats and with its variable precedence and templating, and runs them against Linux hosts over SSH. You do not convert anything, and the same files keep working with `ansible-playbook`.

*Volant* is French for steering wheel: the thing you drive with. Here, it is what you drive your infrastructure with.

> [!WARNING]
> Volant is in **pre-alpha**. It runs real playbooks end to end, but not every playbook yet. Anything it does not support yet is named before it connects to a host, so nothing half-runs.

## Why it is fast

`ansible-playbook` starts a fresh Python interpreter on the host for every task. Volant uploads a small static agent once, keeps one connection per host open for the whole run, and runs Python modules in a warm server that forks a child per task.

On a 35-task playbook against one host, with privilege escalation, median of three runs:

| Engine | Wall clock | Per task |
|---|---|---|
| **Volant** | **7.90 s** | **226 ms** |
| ansible-core with `pipelining = True` | 19.72 s | 564 ms |
| ansible-core with default settings | 27.69 s | 791 ms |

Four published Galaxy roles, `geerlingguy.security`, `nginx`, `git` and `pip`, also run unmodified against real hosts, end on the same recap as ansible-core 2.19.12, and take about half the time of the pipelined reference ([decision record 0007](https://volant.sh/decisions/0007-action-plugins/)). The method and the caveats are on the [home page of the documentation](https://volant.sh/).

## Install

```sh
curl -fsSL https://volant.sh/install.sh | sh
```

The script works on Linux and macOS, x86_64 and arm64. It downloads the newest release, checks its checksum, and installs `volant` under `~/.local`, without root. The binary carries the agents it uploads to your hosts, so `cargo binstall volant` gives the same result. The [installation guide](https://volant.sh/start/installation/) covers its options, what the controller and the hosts need, and the other ways to install.

## A first run

```sh
cat > inventory.ini <<'EOF'
localhost ansible_connection=local
EOF

cat > site.yml <<'EOF'
- hosts: localhost
  gather_facts: false
  tasks:
    - name: Who am I?
      command: id -un
EOF

volant playbook -i inventory.ini site.yml
```

```text
PLAY [localhost] ***************************************************************

TASK [Who am I?] ***************************************************************
changed: [localhost]

PLAY RECAP *********************************************************************
localhost                  : ok=1    changed=1    unreachable=0    failed=0    skipped=0    rescued=0    ignored=0
```

`volant playbook` takes the same options as `ansible-playbook`. The [quickstart](https://volant.sh/start/quickstart/) continues with a remote host over SSH.

## What works today

| | |
|---|---|
| **Plays** | `pre_tasks`, roles with dependencies and argument specs, `tasks`, `post_tasks`, handlers with `listen` and `flush_handlers`, `serial` |
| **Tasks** | `when`, `loop`, `register`, `until`, `changed_when`, `failed_when`, `ignore_errors`, `no_log`, `environment`, tags, `run_once`, `delegate_to` |
| **Structure** | blocks with `rescue` and `always`, `import_*` and `include_*` for tasks, roles and playbooks |
| **Modules** | `command`, `shell`, `raw`, controller modules such as `debug`, `set_fact` and `assert`, most ansible-core Python modules, modules from installed collections, and `copy`, `dnf`, `fetch`, `package`, `reboot`, `service`, `template` and `unarchive` as [action plugins](https://volant.sh/reference/action-plugins/) |
| **Hosts** | static INI inventories, `group_vars` and `host_vars`, the full host-pattern grammar, SSH with keys, `become` through `sudo`, fact gathering |
| **Listings** | `--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check`, byte for byte like Ansible |

Not there yet: a few modules backed by an action plugin (`uri`, `script`), a collection's own action plugins, vault, check mode, YAML inventories. See [Will my playbook run?](https://volant.sh/start/compatibility/) and the [roadmap](https://volant.sh/project/roadmap/).

## Promises

- Your files stay yours: no conversion step and no new format.
- Anything this release does not support yet is named before the first connection, never skipped in silence.
- Linux over SSH comes first. Windows and network devices come later.
- Volant collects no telemetry and makes no network call other than the ones your playbook asks for.

## Contributing

Bug reports, fixes and ideas are welcome. Start with [CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md). Questions go to [Discussions](https://github.com/lorica-labs/volant/discussions), bugs and feature requests to [Issues](https://github.com/lorica-labs/volant/issues), and security problems to the [security policy](https://github.com/lorica-labs/volant/blob/main/SECURITY.md).

## License

Volant is licensed under [GPL-3.0-or-later](https://github.com/lorica-labs/volant/blob/main/LICENSE), like Ansible itself. [Decision record 0001](https://volant.sh/decisions/0001-gpl-3-0-or-later-license/) explains why.
