---
title: Connections and the agent
description: How Volant reaches a host, the host variables and ansible.cfg settings it reads, and how the agent cache works.
---

Volant reaches a managed host, puts a small static agent there, and talks to it over the same connection for the rest of the run.

## Connection types

`ansible_connection` picks the transport. It defaults to `ssh`, as in Ansible.

| Value | What it does |
|---|---|
| `ssh` | Runs the OpenSSH client from your `PATH`. Volant builds the command line itself, without a wrapper and without a Rust SSH library, so your `~/.ssh/config` applies. |
| `local` | Runs the agent as a child process on the controller, with no network. |

Any other value is not supported yet: that host is reported unreachable, with the transport named, and the rest of the run continues.

Only key files and keys held by an ssh-agent work. `ssh` runs with `BatchMode=yes` and never prompts, so a host that would ask for a password or a passphrase is reported unreachable instead of hanging the run. `-k` and `ansible_password` are not implemented, because OpenSSH cannot read a password from its standard input without `sshpass`.

## Host variables

Volant reads these per host, from the inventory or from any other variable source. A test checks this list against the names the connection code reads, so it stays in sync with the engine.

| Variable | Effect |
| --- | --- |
| `ansible_connection` | `ssh` (default) or `local` |
| `ansible_host` | Address to connect to. Defaults to the inventory name. |
| `ansible_port` | `ssh -p`. A quoted number works as well as a bare one. |
| `ansible_user` | `ssh -l` |
| `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_ssh_common_args` | Extra `ssh` arguments, split the way a shell would split them |
| `ansible_ssh_extra_args` | The same, added after the common ones |
| `ansible_remote_tmp` | Directory that holds the agent cache on that host |
| `ansible_become` | Escalate or not, overriding the `become` keyword |
| `ansible_become_user` | Target user for escalation |
| `ansible_become_method` | Must be `sudo` |
| `ansible_become_password`, `ansible_become_pass` | Escalation password |

An unbalanced quote in `ansible_ssh_common_args` or `ansible_ssh_extra_args` fails the host instead of being dropped. That matters behind a bastion: silently discarding a `ProxyCommand` would connect straight to an address that may belong to a different machine.

The `ansible.cfg` settings that affect connections, such as `timeout`, `remote_user`, `host_key_checking` and `forks`, are listed in [Configuration](/reference/configuration/).

## Reaching a host

```mermaid
sequenceDiagram
  participant C as Controller
  participant H as Host
  C->>H: is this agent version cached?
  alt cached and runnable
    H-->>C: version matches
  else missing, outdated or broken
    H-->>C: architecture, what is missing
    C->>H: pipe the agent on stdin
    Note over H: check space, write, verify, rename
  end
  C->>H: start the cached agent
  Note over C,H: the protocol runs over its stdin and stdout
```

## The agent cache

The agent is a single static executable of a few hundred kilobytes. Volant uploads it once per host and reuses it on every later run.

It lives at `<remote_tmp>/volant-agent-<version>/volant-agent`, so with the default `remote_tmp` it sits under `~/.ansible/tmp/` in a home directory. The version in the path is the controller's own, which is how an upgraded controller avoids running an older agent.

Whose home directory depends on who runs the agent. A task that does not escalate runs it as the account you log in as, and caches it there. A task that escalates runs it as the `become_user` and caches it in that user's home, written through `sudo`. This is what makes a `become_user` other than `root` work: home directories are often mode 0700, so an agent cached under your login account would be out of reach for the user you become. A host therefore holds one cache per account the run acts as, each owned by that account.

The agent travels through `ssh -C`, so OpenSSH compresses it on the wire and nothing has to be unpacked on the host.

Before every connection, Volant asks the cached agent for its version. A version mismatch, a missing file or a file that will not start all lead to a fresh upload. A broken cache, such as a copy truncated by an interrupted run, gets repaired instead of reported.

The upload checks free space with `df` first, and wants room for twice the agent plus a megabyte. It writes to a temporary name that includes the remote shell's process ID, checks the byte count, then renames into place. A transfer cut short never lands under the final name, and two runs uploading to the same host at once cannot mix their bytes.

To clear the cache, remove the directory. Nothing else is left behind:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-*'
```

An upload already removes the cache directories of other versions, so a host holds one agent rather than a history of them.

## Persistent connections

A host keeps its connection to the agent for the whole run, across plays. Tasks travel over that one connection instead of opening a new one each time. If the connection dies between two plays, Volant reconnects once, and a second failure reports the host unreachable.

Some tasks add a connection:

- Escalation. A host that uses `become` holds a second connection, to an agent running as the target user. It is kept until the end of the play, so a play of escalated tasks starts one escalated agent, not one per task. A host holds at most one escalated connection at a time: escalating to a different user replaces it.
- Delegation. A delegated task runs over the delegate's own connection, with the delegate's settings. A host whose play delegates to three other hosts holds those three connections alongside its own.

## One connection per host

All the `ssh` runs Volant makes for one inventory host share a single OpenSSH connection: the version check, the agent upload, the link to the agent and the escalated link. Volant adds `-o ControlMaster=auto -o ControlPath=<dir>/<name> -o ControlPersist=30s` to its command line, so only the first run pays for the key exchange and the authentication. The shared connection stays open for 30 seconds after its last session, and a run started in that window reuses it.

The sockets live in `$XDG_RUNTIME_DIR/volant-cm`. Without a usable runtime directory they go to `/tmp/volant-cm-<uid>`, created at mode 0700. Whoever can create a socket there could hand your next `ssh` a connection of their own, so a directory that is not yours, or not at mode 0700, is refused. The run then prints `[WARNING]: ssh connections are not shared` with the reason and carries on with one connection per run.

The socket name is a hash of the inventory name and every `ssh` option the host connects with, plus what `ssh -G` resolves from your configuration files. A connection opened for one user, key, port or `ProxyJump` never serves a host that asks for another, and a `HostName` changed in `~/.ssh/config` between two runs gets a new connection. Two inventory aliases of one machine get two connections, because a single one shared by fifty aliases runs into the server's `MaxSessions` limit.

Volant leaves sharing to you when you already configure it. It adds nothing when:

- `ansible_ssh_common_args` or `ansible_ssh_extra_args` mention `ControlMaster`, `ControlPath` or `ControlPersist`, or pass `-S` or `-M`;
- your ssh configuration already sets one of them for that host, as `ssh -G` reports it;
- the `ssh` arguments ask for debug output with `-v`;
- `[volant] ssh_control_master = false` or `VOLANT_SSH_CONTROL_MASTER=0`, see [Configuration](/reference/configuration/#volant-ssh-control-master).

An escalated connection runs one extra `ssh` session for the `sudo` probe, two when a password is needed. With sharing on, those sessions ride the same connection too.

It is still OpenSSH doing the work, so `~/.ssh/config`, `Match`, `Include`, `ProxyJump` and your ssh-agent apply exactly as they do for `ansible-playbook`.

## File descriptors

Each connection is a separate `ssh` process and costs the controller three file descriptors. With escalation, a run can hold two connections per host: the host's own and one escalated.

At startup Volant raises its own soft limit on open files up to the hard limit, capped at 1,048,576. Most Linux systems ship a hard limit far above what a run needs, so nothing has to be done.

:::caution
A hard limit of 1024 still stops a run at about 330 connections, or about 169 escalating hosts. `ssh` then stops starting, and the hosts it happens to hit are reported unreachable although nothing is wrong with them. In bash, `ulimit -n 1024` lowers the hard limit along with the soft one.
:::

Check both limits before a run that wide:

```sh
ulimit -Sn; ulimit -Hn
```

## Not there yet

- SSH passwords (`-k`, `ansible_password`).
- `connection` as a play, block or task keyword.
- Windows and network devices.

[Keywords](/reference/keywords/) is the full list, generated from the tables the engine reads.
