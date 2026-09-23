---
title: Roles
description: Where Volant looks for roles, how dependencies and argument specs work, and how role variables are layered.
---

A role is static composition. Volant reads its directory while the play compiles and splices its tasks into the list of steps. Nothing about a role waits for a host.

## Finding a role

`roles:` entries, `meta/main.yml` dependencies and `import_role` all name a role the same way. Volant looks in this order:

1. `<playbook_dir>/roles`
2. `roles_path` from `ansible.cfg`, or `ANSIBLE_ROLES_PATH`. Without either, the defaults are `~/.ansible/roles`, `/usr/share/ansible/roles` and `/etc/ansible/roles`.
3. `<playbook_dir>` itself

A three-part name such as `acme.demo.hello` is also looked for under `<collections path>/ansible_collections/acme/demo/roles/hello`.

A role nobody can find stops the run with exit code 1, and the message lists every path that was tried.

## Inside a role

Each directory is read file first, then directory: `tasks/main.yml` if it exists, otherwise `tasks/main/`. On a role entry, `tasks_from`, `vars_from`, `defaults_from` and `handlers_from` pick a file other than `main`.

A role's `handlers/` directory is read with its tasks. At a flush, the role's handlers run before the play's own.

## Dependencies

The `dependencies` in `meta/main.yml` are roles in their own right. They run in front of the role that asked for them, and a dependency two roles share runs once. `allow_duplicates: true` turns that deduplication off for one role.

Deduplication looks at more than the name. The entry's parameters, its `vars:`, its `when:` and its `*_from` selections are all part of its identity:

```yaml
roles:
  - base                    # runs once...
  - base                    # ...because this is the same entry
  - { role: base, p: v }    # runs again: different parameters
```

## Argument specs

`meta/argument_specs.yml` becomes a task of its own. It runs after the role's dependencies and before the role's first task, and it checks the role's arguments against the spec entry named after its tasks file.

The task carries the role's `short_description` after a dash, so `--list-tasks` shows `Validating arguments against arg spec 'main' - The spec role`. It is tagged `always`, so no `--tags` selection drops it.

Checks run in the order Ansible runs them: a missing required argument, then a type, then a value outside `choices`, then an argument the spec does not name. Sub-options, `aliases`, `default` and `mutually_exclusive` are not checked yet.

## Role variables

A role contributes three layers of variables, and they sit at different heights in the [precedence order](/variables/precedence/):

| Layer | Where it sits |
|---|---|
| `defaults/main.yml` | At the bottom, below the inventory's own variables. |
| `vars/main.yml` | Above the play's `vars:`, below a task's. |
| Role parameters, the free keys on an entry such as `{ role: base, p: v }` | Above facts. A role parameter beats a `set_fact` of the same name. |

:::tip
`vars:` written on a role entry is not a role parameter. It travels as an ordinary task keyword. The two spellings look alike, so check which one you used when a role reads a value you did not expect.
:::

A role's `defaults` and `vars` are visible to the whole play, so `pre_tasks` and later roles can read them. For the role's own steps, its values are laid back on top. Role parameters stay inside the role.

## Differences from Ansible

- A role name containing a `.` is tried as a path before it is tried as a collection role, so a role directory with a dot in its name stays reachable.
- `collections_path` from `ansible.cfg` and `ANSIBLE_COLLECTIONS_PATH` are read the same way as `roles_path`. That behavior was not measured against the reference.
- `import_role` does not support `public`, `allow_duplicates` and `rolespec_validate` yet. Ansible honors them, and accepting a `rolespec_validate: false` that Volant then ignored would fail a run that Ansible passes.
- A missing `import_tasks` file is reported with the reference's first two sentences but not its third, which names a Python error number for a module that was never called.
- `import_playbook` written as a task is rejected before the play starts instead of failing under a play banner. The exit code is the same 2.
- A file that imports itself is rejected with exit code 1 and a message of its own. Ansible recurses until the interpreter gives up, at exit code 250 with a long traceback.
- A failing argument-spec check has `"failed": true` in its result, like every controller-side failure in Volant.
