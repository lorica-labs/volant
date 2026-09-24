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

Under `loop_control`, only `loop_var` and `label` are read. The other sub-keys are not supported yet, and a sub-key ansible-core does not know is rejected when the playbook loads. `with_items`, `with_first_found` and `with_fileglob` run; every other `with_*` form is not supported yet: rewrite it with `loop` and a filter. See [Keywords](/reference/keywords/#under-loop_control).

## `with_first_found` and `with_fileglob`

```yaml
- name: Distribute the matching config
  copy:
    src: "{{ item }}"
    dest: /etc/app/config.yml
  with_first_found:
    - "{{ ansible_distribution }}.yml"
    - default.yml

- name: Copy every image
  copy:
    src: "{{ item }}"
    dest: /var/lib/app/images/
  with_fileglob:
    - "images/*.tar.gz"
```

Both loop over a lookup, the way any `with_<name>` form does: `with_first_found` walks its terms until one names a file that exists, and `with_fileglob` collects every match of a glob pattern. `with_first_found` also searches a subdirectory of each search entry before the entry itself: `templates/` when the task's action names `template`, `vars/` when it names `include_vars`, `files/` otherwise, the same directory a plain `template:` or `include_vars:` task would search on its own.

Finding nothing fails the task with the reference's own sentence, `No file was found when using first_found.`, unless the term is a mapping with `skip: true`, in which case the loop runs empty. A term that renders undefined is dropped rather than failing the whole lookup, which is what lets a role read a fact no host has set yet and still reach its `when`. A path either lookup finds that still holds a template marker (`{{`, `{%` or `{#`) is refused by name rather than rendered a second time.

A path a lookup found on the controller is the playbook's own content; an item built in part from a host value, such as a term that reads `ansible_distribution`, is bound as data, the same as an item out of a `loop:` over a registered value.

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

- `retries` is the number of retries after the first attempt, not the total: the task runs up to `retries + 1` times. It is 3 when only `until` is written, for up to four runs.
- A `retries` below 1 turns retrying off: the task runs once and its result has no `attempts`.
- Volant waits `delay` seconds, 5 by default, after each of the first `retries` failed runs, never after the last one.
- The result carries the number of retries actually run under `attempts`, at most `retries`. A task that runs out of retries has failed, even if the module itself succeeded.
- `retries` without `until` repeats the task while its result is failed.
- A task backed by an action plugin (`copy`, `dnf`, `package`...) retries its whole sequence of sub-tasks from the start on each attempt, with a fresh plugin and any file sent again; `until` reads only the last sub-task's result.

:::note
Three retries at the default delay spend fifteen seconds waiting between the four runs, as in Ansible.
:::

A looping task retries each item on its own and finishes one item's attempts before starting the next.

`until`, `retries` and `delay` are rejected on `include_tasks` and `include_role`, before the first connection. Ansible rejects them there too, when it loads the playbook. `include_vars` is an ordinary module and retries like any other task.

## Differences from Ansible

- A retried controller-side task prints the ordinary result and failure message. Ansible uses a format of its own there: `Action failed:`, a `Result was:` line per retry and a `retries` key in the result.
- An `until` expression that cannot be evaluated is reported with the reference's prefix and Volant's own message, the same way a failing `when` is.
- `FAILED - RETRYING` for a task inside a role shows the task's own name rather than `role : name`.
