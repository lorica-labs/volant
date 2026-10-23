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

`<form>` is `-n` or `-k -S -p ''`, depending on a probe described below. Volant adds `-H` only when no `become_flags` is set: a flag list replaces it. Tasks that do not escalate keep using the unprivileged agent. The agent itself knows nothing about escalation. [Decision record 0004](/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why.

The escalated agent is kept until the end of the play, so a play of escalated tasks pays for `sudo` once per host.

## Which setting wins

`become`, `become_user` and `become_method` are accepted on a play, a block and a task. Measured against Ansible:

1. A host's `ansible_become` and `ansible_become_user` variables beat both keywords, in either direction.
2. The task keyword beats the play keyword.
3. The `ansible.cfg` defaults and `-b`, `--become-user` and `--become-method` apply last.

`become_method` is `sudo` or `su`. Any other method is not supported yet, and so are the fully qualified spellings `ansible.builtin.sudo` and `ansible.builtin.su`. Volant stops before a task runs and names the method, rather than escalating through a mechanism you did not ask for. When a run escalates and the method comes from `--become-method`, `ansible.cfg` or `ansible_become_method`, the run ends with exit code 2 before any task.

`become_user` can be a template such as `{{ app_user }}`. One that changes per loop item is refused, because one batch escalates to one user.

`become_exe` and `become_flags` are read from the keyword and from `ansible_become_exe` and `ansible_become_flags`, the host variable winning. Both are split into words the way a shell splits them, and an unbalanced quote fails the task.

## The password

The password comes from one of these:

- `-K` (`--ask-become-pass`), which turns terminal echo off on Unix.
- `--become-password-file`, which reads the first line of a file.
- `ansible_become_password`, `ansible_become_pass`, or the method's own `ansible_sudo_pass` or `ansible_su_pass`, which wins over the other two. [Connections and the agent](/hosts/connections/#host-variables) lists which variable wins.

It is written once to `sudo`'s standard input. It never appears in a command line, a log or a result: the types that carry it print `<redacted>` in debug output, and `sudo`'s own error output is scrubbed of it before it reaches a message.

## The sudo probe

Volant asks `sudo` which form to use instead of assuming one. It tries `sudo -n` first, and falls back to `sudo -k -S -p ''` only when `sudo` itself rejects the request for lack of authentication. [Decision record 0004](/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why a guess would hang the link.

When escalation fails, for a missing password, a wrong one or a `sudoers` rule that forbids the command, the task fails and the host stays reachable. The messages are Ansible's own: `Missing sudo password` or `Incorrect sudo password`.

## su

`su` has no flag to read a password without a terminal, so Volant answers its prompt instead. The password is written once to the prompt, never put in a command line, a log or `-vvv` output. A wrong password fails the task with `Incorrect su password`. A prompt that never comes ends the task with `Timeout (12s) waiting for privilege escalation prompt`, as in ansible-core.

Over a `local` connection `su` runs on a terminal of its own. A wrong password fails the task with `Premature end of stream waiting for become success.` and what `su` printed, as ansible-core does there. The wait for the prompt is always 12 seconds on a local connection, whatever `-T` says.

## Not there yet

- `doas`, `pbrun` and the other methods. They can be added as other ways to start the agent, without a protocol change.
