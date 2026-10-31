---
title: Delegation, meta tasks and environment
description: Running a task once for a batch or on another host, what each meta action does, and how environment and no_log shape a task.
---

This page answers how to run a task somewhere other than where the play is working, how to steer the run itself with `meta`, and how to set environment variables or hide output for a task.

## run_once

`run_once` runs a task on one host of the batch and gives every other host what it produced, including anything it registered.

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

The hosts of the batch meet in front of such a task before any of them runs it. The election has to be the same for everyone, and the other hosts have to wait for the one that runs.

`run_once` can be a template. Each host of the batch renders it in turn, and the first host for which it is true runs the task. A template that answers differently across the batch ends the run with exit 4.

Under `strategy: free` and `host_pinned`, `run_once` is ignored with a warning and the task runs for every host. A `run_once` task that fails there fails only its own host. See [Other strategies](/docs/playbooks/how-a-play-runs/#other-strategies).

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

`action:` and `local_action:` take a string (`command echo hi`), a mapping with `module:` that Ansible deprecates, or a bare module name with the arguments under `args:`. A word such as `module=command` after `action:` names no module. The task fails when a host reaches it, with exit 2, and a `rescue` or `ignore_errors` takes the failure, as in Ansible.

`delegate_to` and `delegate_facts` can be written on a block and on a task, not on a play, which is where Ansible accepts them too. The two keywords combine: a `run_once` task with a `delegate_to` runs once, on the delegate.

## Meta tasks

`meta` asks the engine for something instead of running a module. Every action Ansible 2.19 has runs here, and each one prints a `TASK [meta]` banner. An action Ansible does not have prints the banner, then fails with `invalid meta action requested: <action>` and an `Origin` block, with exit 1, even when its `when` is false. A meta task adds nothing to the recap: its `skipping:` line is not counted either.

| Action | What it does | Whose `when` counts |
|---|---|---|
| `flush_handlers` | Runs the handlers the host has been told about, where the task stands. | Each host's own. |
| `noop` | Nothing. | Ignored, with a warning. |
| `end_host` | The host leaves the play. No more tasks, no `always` sections, no handlers. | Each host's own. |
| `end_role` | The host skips the rest of the role it is in, an `always` section of the role included. | Each host's own. |
| `end_batch` | The batch ends for every host, and the next batch starts. | The first host's. |
| `end_play` | The play ends for every host, with whatever batches are left. | The first host's. |
| `clear_facts` | Forgets the facts modules gathered, on every host of the batch. Facts from `set_fact` stay. | The first host's. |
| `clear_host_errors` | Forgives the failures and the unreachable hosts of the batch. See below. | The first host's. |
| `refresh_inventory` | Reads every inventory source again. See below. | Ignored, with a warning. |
| `reset_connection` | Closes the host's connection, and the shared `ssh` connection with it. The next task opens both again. | Ignored, with a warning. |

"The first host" is the first host of the batch that is still in the play, in inventory order. The actions that read its `when` run once and print one banner. The others run for each host and print one banner per host that is still in the play.

`end_role` outside a role stops the run before it starts, with exit 4 and ansible-core's message.

### Hosts that leave

A host that ends with `end_host`, `end_batch` or `end_play` has not failed, and the recap shows it as it stood. Inside a `rescue`, the failure it rescued counts as rescued and the host stays out of the exit code. A host that had already failed keeps its failure. Later plays run on the host again.

### Forgiving errors

`clear_host_errors` takes the failures of the batch off the exit code. The recap keeps its counts, so a host can show `failed=1` in a run that exits 0.

- A host that failed does not come back to the play it failed in. It is back in the plays after it.
- A host that went unreachable comes back. It picks the play up where it stopped, and runs the steps it missed after every other host is done. If it is unreachable again, it counts again.
- A host that was unreachable in an earlier play also comes back, from the first task of the play that cleared. It is unreachable again in the plays after, and the exit code says 4 either way.

### Refreshing the inventory

`refresh_inventory` reads every inventory source again, in the order of `-i`. `groups` and `hostvars` change at once. A host the sources gained joins from the next play, and a host they lost leaves the play where it stands, without failing. Hosts and groups made by `add_host` and `group_by` stay. If the sources cannot be read any more, the inventory is empty from then on, as in Ansible: every host leaves, and the next plays match nothing.

Facts and `set_fact` values stay as they were. See [Inventories](/docs/hosts/inventories/).

## environment

`environment` sets variables for the process a module runs in.

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

Layers are merged key by key, each one over the previous: the play first, then each enclosing block from the outside in, then the task. Each layer is rendered with the variables of the current loop item.

Values become what Python's `str()` makes of them, so `42` becomes `"42"`, `true` becomes `"True"` and `null` becomes `"None"`. An undefined variable inside `environment` fails the task.

`environment` may also be a list of mappings, merged in order. An entry or a value that is `omit` is left out.

## no_log

`no_log: true` replaces the task's output with the censored line `ansible-playbook` prints, on every line it would have shown and at every verbosity level. That covers the `ok:` and `changed:` lines, `fatal:`, `UNREACHABLE!`, each loop item's line, and `debug` output. A loop item's label becomes `(censored due to no_log)`.

The `[ERROR]:` block printed above a failure keeps its prefix, such as `Task failed: Action failed:`, and shows `<error censored due to no log>` in place of the message. Here Volant differs from `ansible-playbook` 2.19.12, which censors only an unreachable host's reason and prints a failed task's message in full, so a secret in a `fail` message reaches the terminal.

`no_log` can be a template. Each host renders it when the task runs. In a loop, one item that hides hides every item, where ansible-core hides each item by its own value.

The task banner and the task's name are not hidden. The registered variable and the recap keep the real result, so a later task can still read it.

## Differences from Ansible

- `delegate_to` is rendered once per task, not once per loop item. One batch is one message to one agent over one connection, and a per-item delegate would split a loop across connections.
- `local_action` is `delegate_to: localhost` written another way, and wins over a `delegate_to` on the same task.
- When the host elected to run a `run_once` task cannot be reached, the waiting hosts leave the play and the run ends there. Ansible prints one more empty play banner. Neither engine elects another host, and the exit code is the same 4.
- `run_once` combined with `delegate_facts` stores the facts on the delegate, which keeps `delegate_facts` meaning one thing. Ansible's behavior here was not measured.
- `become`, `become_user` and `become_method` written from the loop item are rendered for each item. When every item escalates the same way, the loop runs. When they differ, the task fails with `loop items that escalate differently from one another are not supported yet`, where Ansible escalates per item: one task's items run over one link, for the same reason as the delegate.
- An `environment` value that is not a mapping is skipped with a warning that names the value. Ansible prints the whole stack of layers it sat in instead.
- That warning is not censored by `no_log`. Ansible does not censor its warning either, so this matches ansible-core, but keep it in mind before you put a secret in an `environment` that might not be a mapping.
