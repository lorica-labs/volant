---
title: Delegation and run_once
description: Running a task once for a batch, or on a different host from the one it is for.
---

## run_once

`run_once` runs a task on one host of the batch and gives every other host what it produced, including anything it registered.

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

The hosts of the batch meet in front of such a task before any of them runs it. The election has to be the same for everyone, and the other hosts have to wait for the one that runs.

## delegate_to

`delegate_to` runs a task somewhere other than the host it is running for.

```yaml
- name: Take the host out of the load balancer
  command: /usr/local/bin/lb-drain {{ inventory_hostname }}
  delegate_to: lb1
```

The task travels over the delegate's own connection, with the delegate's connection settings. The result still belongs to the host the play is working on: the recap counts that host, and the result line reads `[web1 -> lb1]`.

- A task skipped by `when` never reached the delegate, so its `skipping:` line has no arrow.
- The delegating host's own connection is not opened for a delegated task, so a task delegated away from an unreachable host still runs.
- The delegate is looked up in the whole inventory, not only the play's hosts. A play over two hosts can delegate to a third that stays out of the recap.
- A name the inventory does not have is treated as an SSH target of that name, and reported unreachable if that fails. An unreachable delegate takes the delegating host out of the run.

`delegate_facts: true` stores what the task produced on the delegate instead of on the hosts that ran it.

A controller-side module such as `set_fact` or `debug` always runs on the controller, whatever `delegate_to` says. The delegate then only decides the name shown on the result line and, with `delegate_facts`, whose facts are written.

`delegate_to` and `delegate_facts` can be written on a block and on a task, not on a play, which is where Ansible accepts them too. The two keywords combine: a `run_once` task with a `delegate_to` runs once, on the delegate.

## Differences from Ansible

- `delegate_to` is rendered once per task, not once per loop item. One batch is one message to one agent over one connection, and a per-item delegate would split a loop across connections.
- `local_action` is not supported yet. `delegate_to: localhost` does the same job and runs.
- When the host elected to run a `run_once` task cannot be reached, the waiting hosts leave the play and the run ends there. Ansible prints one more empty play banner. Neither engine elects another host, and the exit code is the same 4.
- `run_once` combined with `delegate_facts` stores the facts on the delegate, which keeps `delegate_facts` meaning one thing. Ansible's behavior here was not measured.
- `become_user: "{{ item }}"` fails the task, where Ansible escalates per item. Escalation is settled once per batch, for the same reason as the delegate.
