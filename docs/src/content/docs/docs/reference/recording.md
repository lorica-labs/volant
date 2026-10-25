---
title: Run recordings
description: The file the volant.compat.record callback writes while ansible-playbook runs, line by line.
---

Before a team switches, it can record what `ansible-playbook` does on its real runs and have Volant replay those runs later. The `volant.compat.record` callback writes the recording. It ships in the `volant.compat` collection, needs only the Python standard library and runs on ansible-core 2.19 and later.

## Enable it

Install the collection where ansible-core finds its collections, then add two sections to `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

The callback is a notification callback. It never replaces the stdout callback, and the run's output and exit code are the same as without it.

| Setting | Meaning |
|---|---|
| `path` | The file to write. `{{ timestamp }}` becomes the time the run started, in UTC, as `20261025T093000Z`; no other template is read. A name ending in `.gz` is compressed with gzip. Missing parent directories are created with mode `0700`. The default is `shadow/run-{{ timestamp }}.jsonl.gz`. |

`VOLANT_RECORD_PATH` sets `path` from the environment, which suits a CI job.

When the file cannot be written, the callback prints a warning and the run goes on without a recording.

## Security

A recording holds task results as ansible-core saw them. It can contain decrypted vault values, the run's extra variables and everything a module returned. Treat it as a secret:

- The file is created with mode `0600`, and an existing file is set back to `0600`.
- Keep the recording directory out of version control, and store recordings from CI as private artifacts with a short retention.
- A task with `no_log` keeps its status, and its result is recorded as `{"censored": true}`.

## Format

A recording is JSON Lines: one JSON object per line, each with an `event` key. This page describes format `1`.

| `event` | Fields |
|---|---|
| `header` | `format`, `ansible_core` (its version), `cli` (the run's command line options, as ansible-core parsed them), `cwd`, `playbooks` (the `path` and `sha256` of each playbook), `inventory` (`sources` as given, and the `path` and `sha256` of each source that is a file), `extra_vars_hash`, `started` |
| `inventory` | `hosts`: every host of the inventory, with the groups it belongs to besides `all` |
| `play` | `uuid`, `name`, `hosts` (the play's pattern) |
| `task` | `uuid`, `path` (`file:line`), `name`, `action` (the module or action's full name), `handler`, `role` |
| `result` | `host`, `task` (its `uuid`), `item`, `status`, `delegated_to`, `result` |
| `retry` | `host`, `task`, `attempt`, `result`: one failed attempt of an `until` loop |
| `include` | `file`, `hosts`: a file `include_tasks` or `include_role` brought in, and the hosts that run it |
| `notify` | `handler`, `host`: a handler that will run on a host |
| `stats` | `hosts` (the recap of each host), `custom` (what `set_stats` gathered) |

The `header` and `inventory` lines come first, when the first play starts. `stats` is the last line of a complete run, so a recording without it comes from a run that was interrupted.

Notes on the fields:

- Every hash is SHA-256, in hexadecimal. `extra_vars_hash` is computed over the extra variables as JSON with sorted keys.
- `status` is `ok`, `changed`, `failed`, `skipped` or `unreachable`. A failure that `ignore_errors` let through is still `failed`.
- In a loop, each item has a `result` line whose `item` is its index from 0. The line with `item` set to `null` that follows is the whole task, with the items under `results`.
- `delegated_to` is the host a `delegate_to` named, or `null`.
- `result` is the task's result as callbacks receive it, including the keys ansible-core adds, such as `invocation`, `false_condition`, `skip_reason` and `attempts`. Which of those keys are present depends on the version: ansible-core 2.21 no longer passes `invocation` to callbacks.
- The result of fact gathering is recorded in full, because a replay renders templates with it. That is 50 to 100 KB per host, which is why gzip is the default.
- `include_tasks` produces a `task` line and a `result` per host of its own, then an `include` line per file. `import_tasks` produces neither, only the imported tasks.
- `notify` lines are written when the handlers run, after the tasks that notified them.
