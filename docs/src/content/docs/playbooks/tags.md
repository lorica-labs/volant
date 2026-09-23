---
title: Tags and listings
description: How tags select tasks, and the four commands that describe a run without starting it.
---

`tags` can be written on a play, a role entry, a block or a task. The compiler folds outer tags into every task underneath. `--tags` and `--skip-tags` then decide which tasks are compiled in at all, so tags decide both what runs and what the listings show.

## The tag rules

Volant applies Ansible's rules exactly:

| Tag on the task | Behavior |
|---|---|
| `always` | Selected by every `--tags`. Survives `--skip-tags all` unless `always` is itself skipped. |
| `never` | Left out of `--tags all` and `--tags tagged`. Comes back only when a selection names it. |
| no tag | Treated as the tag `untagged`, which is why `--tags untagged` selects it. |

`[tags] run` and `[tags] skip` in `ansible.cfg`, and `ANSIBLE_RUN_TAGS` and `ANSIBLE_SKIP_TAGS`, are defaults. The command line adds to them rather than replacing them.

Tags written on an `include_tasks` or an `include_role` stop at the statement: `--tags inc` runs the include, not what it brings in. The tags of the play, of the role entry and of the blocks around the statement do reach the included tasks, so `--tags nginx` on a role tagged `nginx` runs the files that role includes. This is how Ansible behaves, not a Volant shortcut.

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
