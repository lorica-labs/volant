---
title: Connections and become
description: How Volant reaches a host, the host variables and ansible.cfg settings it reads, and how become works through sudo and su.
---

This page answers how Volant reaches your hosts, which variables control the connection, and how tasks run as another user with `become`.

Volant reaches a managed host, puts a small static agent there (a program that runs your tasks and reports back), and talks to it over the same connection for the rest of the run.

## Connection types

`ansible_connection` picks the transport. It defaults to `ssh`, as in Ansible.

| Value | What it does |
|---|---|
| `ssh` | Runs the OpenSSH client from your `PATH`. Volant builds the command line itself, without a wrapper and without a Rust SSH library, so your `~/.ssh/config` applies. |
| `local` | Runs the agent as a child process on the controller, with no network. |

Every other connection is refused. That covers `paramiko_ssh`, `winrm`, `psrp`, the container and cloud plugins such as `docker`, `podman`, `kubectl` and `aws_ssm`, and the fully qualified spellings of the two supported ones: `ansible.builtin.ssh`, `ansible.builtin.local` and `ansible.legacy.*`. Any value with a dot in it is refused, so a collection's plugin such as `community.docker.docker` is too. The host is reported unreachable with `connection '<name>' is not supported`, and the rest of the run continues.

A bare name that no plugin carries fails the task with `the connection plugin '<name>' was not found`, as Ansible does. The same happens for `docker` and the other names a collection took over from ansible-core, because the collection is not there. That applies whether the value is the host's own connection, a `delegate_to` target's, or that of the host running a `run_once` task.

A password comes from `-k`, from `--connection-password-file` or from the `ansible_password` variable. Volant hands it to `ssh` through `SSH_ASKPASS` and a private temporary file that is removed at the end of the run, which needs OpenSSH 8.4 or later. `ssh` then runs with `BatchMode=no` and `NumberOfPasswordPrompts=1`, so one wrong answer ends the attempt. The password is never on a command line, in the environment of a program or in the output, `-vvvv` included. A vaulted `ansible_password` works like any vaulted variable. Without a password, `ssh` runs with `BatchMode=yes` and never prompts, so a host that would ask for a password or a passphrase is reported unreachable instead of hanging the run.

## Host variables

Volant reads these per host, from the inventory or from any other variable source. A test checks this list against the names the connection code reads, so it stays in sync with the engine.

| Variable | Effect |
| --- | --- |
| `ansible_connection` | `ssh` (default) or `local` |
| `ansible_host`, `ansible_ssh_host` | Address to connect to. Defaults to the inventory name. A value that starts with `-` or holds whitespace or a control character is refused before `ssh` runs. |
| `ansible_port`, `ansible_ssh_port` | `ssh -p`. A quoted number works as well as a bare one. |
| `ansible_user`, `ansible_ssh_user` | `ssh -l` |
| `ansible_password`, `ansible_ssh_pass`, `ansible_ssh_password` | The connection password, offered to `ssh` as described above. |
| `ansible_private_key_file`, `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`, `ansible_ssh_timeout` | Seconds `ssh` waits for the connection, `ssh -o ConnectTimeout`. |
| `ansible_ssh_executable` | The `ssh` program to run. Defaults to `ssh` from your `PATH`. |
| `ansible_ssh_common_args` | Extra `ssh` arguments, split the way a shell would split them |
| `ansible_ssh_extra_args` | The same, added after the common ones |
| `ansible_ssh_use_tty` | Run `raw` and `script` commands on a terminal, as `ssh -tt` does. On unless set to false. The default comes from [`usetty`](/docs/reference/configuration/#ssh-connection-usetty). |
| `ansible_remote_tmp` | Directory that holds the agent cache on that host |
| `ansible_become` | Escalate or not, overriding the `become` keyword |
| `ansible_become_user`, `ansible_sudo_user`, `ansible_su_user` | Target user for escalation |
| `ansible_become_method` | `sudo` or `su` |
| `ansible_become_password`, `ansible_become_pass`, `ansible_sudo_pass`, `ansible_su_pass` | Escalation password |
| `ansible_become_exe`, `ansible_sudo_exe`, `ansible_su_exe` | The escalation program |
| `ansible_become_flags`, `ansible_sudo_flags`, `ansible_su_flags` | Its flags |
| `ansible_su_prompt_l10n` | A list of prompts that `su` may print. It replaces the built-in ones. |

When a host sets a variable and an alias of it, the one Ansible lets win applies. The table lists them from strongest to weakest, with the keyword that sets the same thing:

| Setting | Variables | Keyword |
|---|---|---|
| Address | `ansible_ssh_host`, `ansible_host` | |
| Port | `ansible_ssh_port`, `ansible_port` | `port` |
| User | `ansible_ssh_user`, `ansible_user` | `remote_user` |
| Key file | `ansible_ssh_private_key_file`, `ansible_private_key_file` | |
| Timeout | `ansible_ssh_timeout`, `ansible_timeout` | |
| Password | `ansible_ssh_password`, `ansible_ssh_pass`, `ansible_password` | |
| Connection | `ansible_connection` | `connection` |
| Escalation user, program and flags | `ansible_sudo_*` or `ansible_su_*` (the method's own), then `ansible_become_*` | `become_user`, `become_exe`, `become_flags` |
| Escalation password | `ansible_sudo_pass` or `ansible_su_pass`, then `ansible_become_pass`, then `ansible_become_password` | |

A password variable beats `-k` and `--connection-password-file`, and a variable set to an empty string offers no password at all. A connection variable works at every level that sets variables: inventory, `group_vars`, a play's `vars`, a block, a task's `vars` and `-e`.

The keywords work on a play, a block and a task, and `-c` sets the connection for hosts that name none. The order is the one Ansible uses: a host variable such as `ansible_user` wins, then the task, the block and the play, then the command line and `ansible.cfg`. Facts a host reports about itself never choose its user, port or connection. A keyword written as a template is rendered once per task. These values fail the task: a user name that starts with `-` or holds a space, a quote or a control character, a port outside 1-65535, and a connection name with anything but letters, digits, `_` and `.`. A delegated task uses the task's `remote_user` and `port` unless the delegate sets its own `ansible_user` or `ansible_port`.

An unbalanced quote in `ansible_ssh_common_args` or `ansible_ssh_extra_args` fails the host instead of being dropped. That matters behind a bastion: silently discarding a `ProxyCommand` would connect straight to an address that may belong to a different machine.

The `ansible.cfg` settings that affect connections, such as `timeout`, `remote_user`, `host_key_checking` and `forks`, are listed in [Configuration](/docs/reference/configuration/).

## The agent cache

The agent is a small static program, about 1.2 MB for x86_64 Linux. Volant uploads it once per host and reuses it on every later run, so a host holds only the agent and, when Python modules ran, a payload cache.

It lives at `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`, so with the default `remote_tmp` it sits under `~/.ansible/tmp/` in a home directory. The version in the path is the controller's own, which is how an upgraded controller avoids running an older agent. A task that escalates runs its own agent as the `become_user`, cached in that user's home, so a host holds one cache per account the run acts as.

When Python modules have run, a second directory, `volant-blobs-<version>-<uid>`, sits beside the agent directory in the same `remote_tmp` and holds the module payloads Volant sent.

To clear both, remove the two directories. Nothing else is left behind:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

[The agent on your hosts](/docs/internals/agent/#the-agent-cache) covers the upload, the repair of a broken cache and the permission checks.

## Persistent connections

A host keeps its connection to the agent for the whole run, across plays. Tasks travel over that one connection instead of opening a new one each time. If the connection dies between two plays, Volant reconnects once, and a second failure reports the host unreachable.

Some tasks add a connection:

- Escalation. A host that uses `become` holds a second connection, to an agent running as the target user. It is kept until the end of the play, so a play of escalated tasks starts one escalated agent, not one per task. A host holds at most one escalated connection at a time: escalating to a different user replaces it.
- Delegation. A delegated task runs over the delegate's own connection, with the delegate's settings. A host whose play delegates to three other hosts holds those three connections alongside its own.

## One connection per host

All the `ssh` runs Volant makes for one inventory host share a single OpenSSH connection, so only the first run pays for the key exchange and the authentication. The shared connection stays open for 30 seconds after its last session. OpenSSH does the work, so `~/.ssh/config`, `Match`, `Include`, `ProxyJump` and your ssh-agent apply exactly as they do for `ansible-playbook`.

Volant leaves sharing to you when your own `ssh` arguments or configuration already set `ControlMaster`, `ControlPath` or `ControlPersist` for the host. Set `[volant] ssh_control_master = false` or `VOLANT_SSH_CONTROL_MASTER=0` to turn it off, see [Configuration](/docs/reference/configuration/#volant-ssh-control-master).

Each connection costs the controller three file descriptors. A hard limit of 1024 open files stops a run at about 330 connections, or about 169 escalating hosts. [The agent on your hosts](/docs/internals/agent/#one-connection-per-host) has the socket location, the keepalive settings and the check to run before a wide run.

## Become

`become` runs tasks as another user through `sudo` or `su`.

```yaml
- hosts: web
  become: true
  tasks:
    - name: Install nginx
      apt:
        name: nginx
        state: present

    - name: Check the app as its own user
      command: /opt/app/bin/healthcheck
      become_user: app
```

### How it works

Volant does not wrap each task in `sudo`. It starts a second agent for the target user:

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` is `-n` or `-k -S -p ''`, depending on a probe described below. Volant adds `-H` only when no `become_flags` is set: a flag list replaces it. Tasks that do not escalate keep using the unprivileged agent. The agent itself knows nothing about escalation. [Decision record 0004](/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why.

The escalated agent is kept until the end of the play, so a play of escalated tasks pays for `sudo` once per host.

### Which setting wins

`become`, `become_user` and `become_method` are accepted on a play, a block and a task. Measured against Ansible:

1. A host's `ansible_become` and `ansible_become_user` variables beat both keywords, in either direction.
2. The task keyword beats the play keyword.
3. The `ansible.cfg` defaults and `-b`, `--become-user` and `--become-method` apply last.

`become_method` is `sudo` or `su`. Any other method is not supported yet, and so are the fully qualified spellings `ansible.builtin.sudo` and `ansible.builtin.su`. Volant stops before a task runs and names the method, rather than escalating through a mechanism you did not ask for. When a run escalates and the method comes from `--become-method`, `ansible.cfg` or `ansible_become_method`, the run ends with exit code 2 before any task.

`become_user` can be a template such as `{{ app_user }}`. One that changes per loop item is refused, because one batch escalates to one user.

`become_exe` and `become_flags` are read from the keyword and from `ansible_become_exe` and `ansible_become_flags`, the host variable winning. Both are split into words the way a shell splits them, and an unbalanced quote fails the task.

### The become password

The password comes from one of these:

- `-K` (`--ask-become-pass`), which turns terminal echo off on Unix.
- `--become-password-file`, which reads the first line of a file.
- `ansible_become_password`, `ansible_become_pass`, or the method's own `ansible_sudo_pass` or `ansible_su_pass`, which wins over the other two. [Host variables](#host-variables) lists which variable wins.

It is written once to `sudo`'s standard input. It never appears in a command line, a log or a result: the types that carry it print `<redacted>` in debug output, and `sudo`'s own error output is scrubbed of it before it reaches a message.

### The sudo probe

Volant asks `sudo` which form to use instead of assuming one. It tries `sudo -n` first, and falls back to `sudo -k -S -p ''` only when `sudo` itself rejects the request for lack of authentication. [Decision record 0004](/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why a guess would hang the link.

When escalation fails, for a missing password, a wrong one or a `sudoers` rule that forbids the command, the task fails and the host stays reachable. The messages are Ansible's own: `Missing sudo password` or `Incorrect sudo password`.

### su

`su` has no flag to read a password without a terminal, so Volant answers its prompt instead. The password is written once to the prompt, never put in a command line, a log or `-vvv` output. A wrong password fails the task with `Incorrect su password`. A prompt that never comes ends the task with `Timeout (12s) waiting for privilege escalation prompt`, as in ansible-core.

Over a `local` connection `su` runs on a terminal of its own. A wrong password fails the task with `Premature end of stream waiting for become success.` and what `su` printed, as ansible-core does there. The wait for the prompt is always 12 seconds on a local connection, whatever `-T` says.

## Not there yet

- Windows hosts and network devices.
- The connection plugins named under Connection types.
- `doas`, `pbrun` and the other become methods. They can be added as other ways to start the agent, without a protocol change.

[Keywords](/docs/reference/keywords/) is the full list, generated from the tables the engine reads.
