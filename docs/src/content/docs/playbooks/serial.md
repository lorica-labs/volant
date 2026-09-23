---
title: Serial batches
description: How serial splits a play into batches of hosts, and when a run stops.
---

`serial` splits a play's hosts into batches, in inventory order, and plays each batch in turn. Each batch gets its own banner, its own handler flushes and its own `run_once` election.

```yaml
- hosts: web
  serial:
    - 1       # one canary first
    - "25%"   # then a quarter of the hosts at a time
  tasks:
    - command: /opt/app/upgrade.sh
```

| Value | Meaning |
|---|---|
| A number | That many hosts per batch. |
| A string ending in `%` | A share of the whole host list, not of what is left. Truncated rather than rounded, and never less than one host. |
| A list | One size per batch. The last element repeats once the list runs out. |
| `0`, `-1`, `[]`, or more than the host count | One batch holding every host. |

## When the run stops

The run stops at the first batch that had live hosts and lost all of them, which is where Ansible stops too. The exit code is 2 when they failed and 4 when they all became unreachable.

A batch whose hosts had already failed in an earlier play does not count as that. It prints its banner and the run carries on.

Inside a batch, `ansible_play_batch` and the deprecated `play_hosts` name the batch, while `ansible_play_hosts` names the whole play. `vars_files` is rendered and read again for each batch.

## Differences from Ansible

- A `serial` that is not a number, such as `serial: abc` or `"50% "` with a trailing space, is rejected with exit code 4 and the reference's message. Ansible crashes with exit code 250, which is Ansible reporting a bug in itself rather than rejecting a playbook.
- An undefined variable in `serial` is reported with the reference's prefix and Volant's own message, with the same exit code 4.
- `play_hosts` holds what Ansible puts there, but Volant prints no deprecation warning for it.
