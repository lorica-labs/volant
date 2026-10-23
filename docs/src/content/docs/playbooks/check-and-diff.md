---
title: Check mode, diff and listings
description: What --check and --diff do, which modules answer them, how tags select tasks, and the commands that describe a run without starting it.
---

This page answers how to preview a run before it changes anything: dry runs with `--check`, file changes with `--diff`, task selection with tags, and listings that connect to nothing.

`--check` reports what each task would change without changing it. `--diff` prints the changes a module makes to files. Both work like `ansible-playbook` 2.19.12, and the recap and exit code match.

The task keywords win over the options. `check_mode: false` runs the task for real under `--check`. `check_mode: true` makes one task a dry run in a normal play. `diff: false` keeps one task's changes off the screen under `--diff`.

`ansible_check_mode` and `ansible_diff_mode` report the command-line `--check` and `--diff` only. A play's or a task's own `check_mode` or `diff` keyword does not change them.

## What runs where

A task in check mode never changes a host, whichever path runs it.

Volant carries a native version of some modules, written in Rust and run inside the agent, the small program it uploads to each host. When the native version cannot answer a task, it hands the task back: the module's own Python code runs on the host instead.

| Task | What happens |
|---|---|
| `command`, `shell`, `raw` | Skipped, as in ansible-core. A `command` or `shell` with `creates` or `removes`, in the line or in `args:`, is checked: `ok` when the guard stops it, `changed` when it would run. |
| `stat`, `setup`, `package_facts`, `service_facts` | Answered by the agent. They only read, so check and diff change nothing. |
| `file`, `lineinfile`, `systemd`, `user`, `group`, `apt` | Handed back to the module's Python code on the host, which knows what it would have changed and how to word the diff. |
| `copy`, `template`, `fetch`, `unarchive` and other action plugins | Run by ansible-core's own plugin through the controller's Python helper (a process running ansible-core on the controller), which computes the check result and the diff itself. |
| Controller modules (`debug`, `set_fact`, `assert`, ...) | Run as usual. |
| `package`, `service`, `dnf` | Pick the module for the host, then that module runs in check mode. |

The profile (`--profile`) names the reason each hand-back happened: "the task runs in check mode" or "the task shows a diff".

## A host with no Python

Handing back needs Python on the host. Without it, every native module that changes the host fails under `--check` or `--diff`: `apt`, `user`, `group`, `file`, `lineinfile`, `systemd`, `systemd_service` and the `copy` module that the copy plugin runs. Install Python on the host, or run the play without those options. Tasks that only read (`stat`, `setup`, `package_facts` and `service_facts`), and `command`, `shell` and `raw`, still work.

## Diff output

A diff prints only for a task that changed something. `copy` and `template` print nothing when `diff` is off. The helper process starts only when a diff is printed.

## Known gaps

- `--step` does not ask before a `meta` or `flush_handlers` step. This has not been compared with ansible-core.
- `-vv` does not print the handler notification lines.
- `-vvvv` lists the options ansible-core lists, in its order, and the plugin set-up lines. Its ssh lines show the commands this engine runs over `ssh` (the probe, the upload and the link to the agent), not the `sftp` upload of ansible-core, and a local connection or a module file prints no line.
- A task with a missing key on a skipped result gives a different message from ansible-core.

The [parity page](/reference/parity/) lists every option and keyword with its status.

## Tags

`tags` can be written on a play, a role entry, a block or a task. The compiler folds outer tags into every task underneath. `--tags` and `--skip-tags` then decide which tasks are compiled in at all, so tags decide both what runs and what the listings show.

### The tag rules

Volant applies Ansible's rules exactly:

| Tag on the task | Behavior |
|---|---|
| `always` | Selected by every `--tags`. Survives `--skip-tags all` unless `always` is itself skipped. |
| `never` | Left out of `--tags all` and `--tags tagged`. Comes back only when a selection names it. |
| no tag | Treated as the tag `untagged`, which is why `--tags untagged` selects it. |

`[tags] run` and `[tags] skip` in `ansible.cfg`, and `ANSIBLE_RUN_TAGS` and `ANSIBLE_SKIP_TAGS`, are defaults. The command line adds to them rather than replacing them.

Tags written on an `include_tasks` or an `include_role` stop at the statement: `--tags inc` runs the include, not what it brings in. The tags of the play, of the role entry and of the blocks around the statement do reach the included tasks, so `--tags nginx` on a role tagged `nginx` runs the files that role includes. Ansible behaves the same way.

### `ansible_run_tags` and `ansible_skip_tags`

A task can read its own run's tag selection back as two variables: `ansible_run_tags` holds `--tags` (`["all"]` when it was not given) and `ansible_skip_tags` holds `--skip-tags` (`[]` when it was not given), each after `[tags] run` / `[tags] skip` of `ansible.cfg` and the two `ANSIBLE_RUN_TAGS` / `ANSIBLE_SKIP_TAGS` environment variables are folded in.

```yaml
- name: Only fetch the kubeconfig once it has been asked for
  fetch:
    src: /etc/rancher/k3s/k3s.yaml
    dest: "{{ kubeconfig }}"
  when: "'kubeconfig' in ansible_run_tags"
```

## Look before you run

Four options read a playbook and print what a run would do, without connecting to anything:

```sh
volant playbook -i inventory.ini site.yml --list-hosts
volant playbook -i inventory.ini site.yml --list-tasks
volant playbook -i inventory.ini site.yml --list-tags
volant playbook -i inventory.ini site.yml --syntax-check
```

Their output matches Ansible's down to the whitespace. The test suite compares it byte for byte with ansible-core over a corpus of playbooks.

Listings walk a block's body only, so a task in `rescue:` or `always:` does not appear, just as in Ansible.

## Differences from Ansible

- A play with several tags prints them sorted, and `--list-hosts` prints a play's hosts in inventory order. Ansible prints a Python set in both places, so its order changes from one run to the next.
- A listing does not resolve modules. A playbook whose modules this release cannot run still lists and still passes `--syntax-check`, so you can read a role before the release that runs it.
- A task whose `ignore_errors` or `become` is neither true nor false makes a listing exit with code 4. Ansible lists it and exits 0. The value is a malformed playbook, not an unimplemented feature.
