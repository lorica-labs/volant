---
title: Check mode and diff
description: What --check and --diff do under Volant, which modules answer them on the host, and which hand back to Python.
---

`--check` reports what each task would change without changing it. `--diff` prints the changes a module makes to files. Both work like `ansible-playbook` 2.19.12, and the recap and exit code match.

The task keywords win over the options. `check_mode: false` runs the task for real under `--check`. `check_mode: true` makes one task a dry run in a normal play. `diff: false` keeps one task's changes off the screen under `--diff`.

## What runs where

A task in check mode never changes a host, whichever path runs it.

| Task | What happens |
|---|---|
| `command`, `shell`, `raw` | Skipped, as in the reference. |
| `stat`, `setup`, `package_facts`, `service_facts` | Answered by the agent. They only read, so check and diff change nothing. |
| `file`, `lineinfile`, `systemd`, `user`, `group`, `apt` | Handed back to the module's Python code on the host, which knows what it would have changed and how to word the diff. |
| `copy`, `template`, `fetch`, `unarchive` and other action plugins | Run by ansible-core's own plugin through the controller's Python helper, which computes the check result and the diff itself. |
| Controller modules (`debug`, `set_fact`, `assert`, ...) | Run as usual. |
| `package`, `service`, `dnf` | Pick the module for the host, then that module runs in check mode. |

The profile (`--profile`) names the reason each hand-back happened: "the task runs in check mode" or "the task shows a diff".

## A host with no Python

Handing back needs Python on the host. Without it, `apt`, `user`, `group` and `file` fail under `--check` or `--diff`. Install Python on the host, or run the play without those options. Tasks that only read, and `command`, `shell` and `raw`, still work.

## Diff output

A diff prints only for a task that changed something. `copy` and `template` print nothing when `diff` is off. The helper process starts only when a diff is printed.

## Known gaps

- `--step` does not ask before a `meta` or `flush_handlers` step. This has not been compared with the reference.
- `-vv` does not print the handler notification lines.
- `-vvvv` prints fewer options and no per-task ssh or plugin-path lines.
- A task with a missing key on a skipped result gives a different message from the reference.

The [parity page](/reference/parity/) lists every option and keyword with its status.
