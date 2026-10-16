---
title: Privilege escalation
description: How become works through sudo and su, where the password comes from, and which settings win.
---

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

## How it works

Volant does not wrap each task in `sudo`. It starts a second agent for the target user:

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` is `-n` or `-k -S -p ''`, depending on a probe described below. Tasks that do not escalate keep using the unprivileged agent. The agent itself knows nothing about escalation. [Decision record 0004](/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why.

The escalated agent is kept until the end of the play, so a play of escalated tasks pays for `sudo` once per host.

## Which setting wins

`become`, `become_user` and `become_method` are accepted on a play, a block and a task. Measured against Ansible:

1. A host's `ansible_become` and `ansible_become_user` variables beat both keywords, in either direction.
2. The task keyword beats the play keyword.
3. The `ansible.cfg` defaults and `-b`, `--become-user` apply last.

`become_method` is `sudo` or `su`. Any other method is not supported yet: Volant stops before a task runs and names the method, rather than escalating through a mechanism you did not ask for. `become_exe` and `become_flags` are read from the keyword and from `ansible_become_exe` and `ansible_become_flags`, the host variable winning.

## The password

The password comes from `-K` (`--ask-become-pass`), which turns terminal echo off on Unix, or from `ansible_become_password` / `ansible_become_pass`.

It is written once to `sudo`'s standard input. It never appears in a command line, a log or a result: the types that carry it print `<redacted>` in debug output, and `sudo`'s own error output is scrubbed of it before it reaches a message.

## The sudo probe

Volant asks `sudo` which form to use instead of assuming one. A `sudo` that needs no authentication, because of a `NOPASSWD` rule or a cached timestamp, does not read its standard input at all. A password written there would stay on the pipe and be read by the agent as protocol data. So Volant tries `sudo -n` first, and falls back to `sudo -k -S -p ''` only when `sudo` itself rejects the request for lack of authentication.

When escalation fails, for a missing password, a wrong one or a `sudoers` rule that forbids the command, the task fails and the host stays reachable. The messages are Ansible's own: `Missing sudo password` or `Incorrect sudo password`.

## su

`su` has no flag to read a password without a terminal, so Volant answers its prompt instead. The password is written once to the prompt, never put in a command line, a log or `-vvv` output. A wrong password fails the task with `Incorrect su password`. A prompt that never comes ends the task with `Timeout (12s) waiting for privilege escalation prompt`, as in ansible-core.

## Not there yet

- `doas`, `pbrun` and the other methods. They can be added as other ways to start the agent, without a protocol change.
