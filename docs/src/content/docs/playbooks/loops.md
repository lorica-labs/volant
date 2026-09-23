---
title: Loops and retries
description: loop, with_items and loop_control, and how until, retries and delay repeat a task.
---

## Loops

```yaml
- name: Create the service accounts
  command: useradd --system {{ item }}
  loop:
    - app
    - worker
  loop_control:
    label: "{{ item }}"
  register: accounts
```

`loop` and `with_items` cannot both appear on the same task. `with_items` flattens one level of nested lists and `loop` does not, as in Ansible. Registering a looped task collects a `results` list with one entry per item.

Under `loop_control`, only `loop_var` and `label` are read. The other sub-keys are not supported yet, and a sub-key ansible-core does not know is rejected when the playbook loads. Every `with_*` form other than `with_items` is not supported yet: rewrite it with `loop` and a filter. See [Keywords](/reference/keywords/#under-loop_control).

## Retrying with until

```yaml
- name: Wait for the service to answer
  command: curl -fsS http://localhost:8080/health
  register: health
  until: health.rc == 0
  retries: 10
  delay: 3
```

`until` runs a task again until its expression holds.

- `retries` is the total number of attempts, not the number of extra ones. It is 3 when only `until` is written.
- A `retries` below 1 turns retrying off: the task runs once and its result has no `attempts`.
- Volant waits `delay` seconds, 5 by default, after every failed attempt, including the last one.
- The result carries the number of attempts under `attempts`. A task that runs out of attempts has failed, even if the module itself succeeded.
- `retries` without `until` repeats the task while its result is failed.

:::note
Because the delay also follows the last attempt, three attempts at the default delay spend fifteen seconds waiting, as in Ansible.
:::

A looping task retries each item on its own and finishes one item's attempts before starting the next.

`until`, `retries` and `delay` are rejected on `include_tasks` and `include_role`, before the first connection. Ansible rejects them there too, when it loads the playbook. `include_vars` is an ordinary module and retries like any other task.

## Differences from Ansible

- A retried controller-side task prints the ordinary result and failure message. Ansible uses a format of its own there: `Action failed:`, a `Result was:` line per retry and a `retries` key in the result.
- An `until` expression that cannot be evaluated is reported with the reference's prefix and Volant's own message, the same way a failing `when` is.
- `FAILED - RETRYING` for a task inside a role shows the task's own name rather than `role : name`.
