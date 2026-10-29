---
title: Roles, includes and imports
description: Where Volant looks for roles, how dependencies, argument specs and role variables work, and how imports and includes bring other files into a play.
---

This page answers where Volant finds a role, how its dependencies and variables behave, and how `import_*` and `include_*` pull in other files. Volant reads a role's directory while the play compiles and inserts its tasks into the list of steps, so nothing about a role waits for a host.

## Finding a role

`roles:` entries, `meta/main.yml` dependencies and `import_role` all name a role the same way. Volant looks in this order:

1. `<playbook_dir>/roles`
2. `roles_path` from `ansible.cfg`, or `ANSIBLE_ROLES_PATH`. Without either, the defaults are `~/.ansible/roles`, `/usr/share/ansible/roles` and `/etc/ansible/roles`.
3. `<playbook_dir>` itself

A relative `roles_path` entry is read against the directory of `ansible.cfg`, or against the working directory when it comes from `ANSIBLE_ROLES_PATH`. Every entry is made absolute, with `.` and `..` removed, before it is used. A role's `ansible_search_path` and the `included:` lines show that path, as ansible-core does.

A three-part name such as `acme.demo.hello` is also looked for under `ansible_collections/acme/demo/roles/hello`, first in a `collections/` directory beside the playbook, then in each `collections_path` entry.

A role nobody can find stops the run with exit code 1, and the message lists every path that was tried.

## Inside a role

`defaults/` and `vars/` are read file first, then directory: `vars/main.yml` if it exists, otherwise every file of `vars/main/`, in name order. `tasks/`, `handlers/` and `meta/` take a single file and never a directory. On a role entry, `tasks_from`, `vars_from`, `defaults_from` and `handlers_from` pick a file other than `main`.

The name is a path under the matching directory, with or without its extension (`install`, `install.yml`, `sub/install`). A name that leaves that directory, such as `../../../x` or an absolute path outside it, stops the run with `Failed loading '...' for role (name) as it is not inside the expected role path`. Volant follows a symbolic link inside the directory wherever it points, as ansible-core does.

A role's `handlers/` directory is read with its tasks. At a flush, the role's handlers run before the play's own.

## Dependencies

The `dependencies` in `meta/main.yml` are roles in their own right. They run in front of the role that asked for them, and a dependency two roles share runs once. `allow_duplicates: true` turns that deduplication off for one role. This also holds across statements: a role the play's `roles:` ran, or an earlier `include_role` brought in for a host, is not run again as a dependency of a role an `import_role` or an `include_role` brings in later.

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

A role contributes three layers of variables, and they sit at different heights in the [precedence order](/docs/variables/precedence/):

| Layer | Where it sits |
|---|---|
| `defaults/main.yml` | At the bottom, below the inventory's own variables. |
| `vars/main.yml` | Above the play's `vars:`, below a task's. |
| Role parameters, the free keys on an entry such as `{ role: base, p: v }` | Above facts. A role parameter beats a `set_fact` of the same name. |

:::tip
`vars:` written on a role entry is not a role parameter. It travels as an ordinary task keyword. The two spellings look alike, so check which one you used when a role reads a value you did not expect.
:::

A role's `defaults` and `vars` are visible to the whole play, so `pre_tasks` and later roles can read them. For the role's own steps, its values are laid back on top. Role parameters stay inside the role.

An `import_role` with `public: false` keeps them to itself, and an `include_role` does that unless it says `public: true`. See [Role options](#role-options).

## Imports and includes

Five statements bring work in from another file. They fall into two families:

| Statement | Kind | When Volant reads the file | What a listing shows |
|---|---|---|---|
| `import_tasks` | static | while the play compiles | every task inside |
| `import_role` | static | while the play compiles | every task inside |
| `import_playbook` | static | while the play compiles | every play inside |
| `include_tasks` | dynamic | when a host reaches the statement | the statement only |
| `include_role` | dynamic | when a host reaches the statement | the statement only |

A dynamic include is resolved per host, because its file name can depend on that host's variables. The new steps are inserted right behind the statement while the play runs.

### Static imports

An `import_tasks` path is looked for next to the importing file first, then in the role's own `tasks/` directory.

An `import_tasks` path may hold a template. It renders while the play compiles, against `role_path`, `playbook_dir`, the play's `vars:`, the `-e` variables and the roles' `defaults` and `vars`, so `{{ role_path }}/tests/main.yml` works. There is no host yet, so an inventory variable is undefined there and stops the load with exit 4.

`import_playbook` inserts the other file's plays where the statement stands, and each imported play keeps the directory of the file it came from. Its file name renders with the `-e` variables and through the [Python path for filters and lookups](/docs/variables/templating/#filters-tests-and-lookups-that-need-python), so `{{ which }}.yml` works with `-e which=sub.yml`, and so does a name built with `lookup('env', ...)` or a collection filter. Nothing has an inventory yet, so a name cannot depend on a host. A name that reads an undefined variable stops the load with exit 4: `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`.

Three keywords of the `import_playbook` entry reach every play of the file:

- `tags` are added to the tags of each imported play, so `--tags` and `--skip-tags` select its tasks and the listings show them.
- `vars` are added to the variables of each imported play and win over the play's own.
- `when` is put in front of the conditions of every task, role and block of each imported play, and in front of its fact gathering. It is not evaluated once for the whole import. A false condition still opens each play and still prints a skip for each task and host. Handlers are not conditional.

The other keywords that ansible-core 2.19 accepts on the entry, such as `ignore_errors`, `become` and `environment`, do nothing there, and Volant ignores them the same way. Any other key is refused. A `tags` or a `when` that holds a template is refused too, because Volant has no variables when it loads the playbook.

### Dynamic includes

Of what the statement itself says, only its `vars:` travel into the included file. Those variables enter at the role-parameter level of the [precedence order](/docs/variables/precedence/), above facts and below `--extra-vars`, and they accumulate through nested includes. The statement's own `tags` stop at the statement, and its `when` is evaluated for the statement alone.

What the layers around the statement say does reach the included tasks: the play, the role entry that brought the file in, and the blocks around it. Their `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` and `vars` apply to every included task, and a task's own value wins over an inherited one. This is ansible-core 2.19's rule, which skips a dynamically loaded parent and asks the grandparent. An inherited `check_mode: true` is refused in the included file rather than dropped.

An included task lands in the block and section where the statement was written, so a failing include inside a block body is caught by that block's `rescue`.

Volant checks what an include brings in before it runs. By then the hosts are connected and earlier tasks have run, so a module or keyword this release does not support yet can no longer stop the whole run. It fails the include statement for the host that reached it instead, nothing in the file runs, and a `rescue` around the statement can catch it.

Before the first connection, Volant collects the Python modules named by every task and handler file of the roles in play. A module named only in `setup-{{ ansible_os_family }}.yml` is therefore already on the host when the include runs. A file under a role's `tasks/` or `handlers/` that does not parse stops the run before anything connects, for `roles:` and `import_role`. For an `include_role` this early scan does not read the role: earlier plays and tasks run, then the run ends with exit 4 after the statement's banner, and a `when: false` on the include hides the failure. See [The warm Python path](/docs/internals/python/#how-it-works).

A `notify` in an included file that names no handler ends the run with exit code 1 and the same sentence the pre-flight check gives, as ansible-core does. The pre-flight check is the scan Volant runs before it connects to any host. No `ignore_errors` or `rescue` can catch this. See [Blocks and handlers](/docs/playbooks/blocks/#differences-from-ansible).

#### Putting keywords on what an include brings in

`apply:` on `include_tasks` and `include_role` puts keywords on every task the statement brings in, and on the handlers of an included role. It takes the keywords of a block: `tags`, `when`, `vars`, `become`, `environment`, `ignore_errors`, `delegate_to` and the rest. The imports refuse `apply`, since they already pass the keywords you write on them.

```yaml
- include_tasks:
    file: install.yml
    apply:
      tags: [install]
      become: true
  tags: [always]
```

The tags in `apply` are on the included tasks and not on the statement. With `--tags install`, the statement runs only if it carries a tag that the selection keeps, here `always`, and then the included tasks run. A `when` in `apply` is checked for each included task, and a `when` on the statement is checked once. The `vars` in `apply` are block variables, so the statement's own `vars:` win over them.

A key of `apply` that a block does not have stops the run with exit code 4 when the statement runs, after its banner, as in ansible-core. An `apply` that is not a mapping stops the load.

#### Role options

With `public: true`, the `defaults` and `vars` of an `include_role` are visible to everything that runs after the statement, and to the other hosts too, because ansible-core keeps the list of public roles in the play and not in the host. A task that ran before the statement reads nothing of it, and a `when: false` include lends nothing.

With `allow_duplicates: false`, the statement shows its `included:` line and runs nothing for a host that has already had an identical role brought in. Two statements are identical when they name the same role, the same files and the same `vars:`, and for a loop the same item under the same loop variable. `rolespec_validate: false` leaves out the argument-spec check.

Each option of the four statements, as read by Volant and by ansible-core 2.19.12:

| Statement | Option | Default | Volant |
|---|---|---|---|
| `include_role` | `name`, `role` | required | works |
| `include_role` | `tasks_from`, `vars_from`, `defaults_from`, `handlers_from` | `main` | works |
| `include_role` | `public` | `false` | works |
| `include_role` | `allow_duplicates` | `true` | works |
| `include_role` | `rolespec_validate` | `true` | works |
| `include_role` | `apply` | none | works |
| `import_role` | `name`, `role`, the four `*_from` | as above | works |
| `import_role` | `public` | `true` | works: `false` keeps the variables to the role |
| `import_role` | `allow_duplicates` | `true` | works, decided while the play compiles |
| `import_role` | `rolespec_validate` | `true` | works |
| `import_role` | `apply` | none | refused as in ansible-core, `Invalid options for import_role: apply` |
| `include_tasks` | `file` or the free-form name | required | works |
| `include_tasks` | `apply` | none | works |
| `import_tasks` | `file` or the free-form name | required | works |
| `import_tasks` | `apply` | none | refused as in ansible-core, `Invalid options for import_tasks: apply` |

A value that is not a boolean in `public`, `allow_duplicates` or `rolespec_validate` fails the task, or stops the load for an `import_role`, with ansible-core's sentence.

### include_vars

`include_vars` produces variables rather than steps, so it is neither static nor dynamic. It is an ordinary controller-side module, and what it reads lands under `set_fact` and registered results, as in Ansible.

| Question | Answer |
|---|---|
| What does it read? | One file, named by `file:` or written as the free-form path, or every file of a directory, named by `dir:`. The two forms take different options, and giving options of both fails the task. |
| Where is a file looked for? | In `vars/` and then the directory itself, for a role and then for the playbook. An absolute path is used as it is. |
| Where is a `dir:` looked for? | It is not searched for. A task of a role reads it under the role's `vars/`, and `dir: vars/x` names a directory of the role itself when that exists. Any other task reads it against the directory of the file that holds the task. |
| In what order is a directory read? | Directories sorted by their full path as text, then the files of each directory sorted by name. Hidden files and directories are read, and links to directories are followed. |
| What if two files set the same key? | A later file replaces the keys of an earlier one whole. |
| What is in the result? | `ansible_facts` and `ansible_included_var_files`, the files in the order they were read. A file with nothing in it adds nothing, and it is still listed. |
| What about a vaulted file? | A task that reads one is hidden like a `no_log` task: `-v` shows the censored line, and a loop item that read one shows `(censored due to no_log)`. |
| Check mode? | The task runs in check mode, as it does in Ansible. |
| What fails? | A missing directory, a path that is not a directory, a file without a valid extension and a file that is not a mapping fail the task with `message` set, as in Ansible. A failure in a directory keeps the variables of the directories before the failing one in the result, but a failed task sets no variables. A file that cannot be parsed or decrypted raises the error instead, and the task has no result of its own. An option of the wrong type fails with Python's own sentence. |

The options of the `dir:` form:

| Option | Meaning |
|---|---|
| `dir` | The directory to read. |
| `depth` | How many levels to read, the directory being the first. `0`, the default, reads them all. |
| `files_matching` | A regular expression searched in each file name, not in its path. Files that do not match are left out. |
| `ignore_files` | A list of regular expressions, or one string of them split on white space. Each is anchored at the end of the name. Files that match are left out. |
| `extensions` | The extensions a file may have, without the dot. The default is `yaml`, `yml` and `json`. The comparison is case-sensitive. |
| `ignore_unknown_extensions` | Leave out a file with another extension. Without it, such a file fails the task. |
| `name` | Put everything under this one variable. It also works for a single file. |
| `hash_behaviour` | `merge` lays each variable over the variable of that name that the task already sees, mappings merged all the way down. Any other value replaces. |

### Nesting limit

A statement 32 levels deep is rejected. One counter covers role nesting, `import_role` inside a role's tasks and `import_tasks` inside an imported file, because a cycle can run through any mix of the three.

## Differences from Ansible


- A role name containing a `.` is tried as a path before it is tried as a collection role, so a role directory with a dot in its name stays reachable.
- `collections_path` from `ansible.cfg` and `ANSIBLE_COLLECTIONS_PATH` are read the same way as `roles_path`. That behavior was not measured against ansible-core. The `collections/` directory beside the playbook was.
- A missing `import_tasks` file is reported with ansible-core's first two sentences but not its third, which names a Python error number for a module that was never called.
- `import_playbook` written as a task is rejected before the play starts instead of failing under a play banner. The exit code is the same 2.
- A file that imports itself is rejected with exit code 1 and a message of its own. Ansible recurses until the interpreter gives up, at exit code 250 with a long traceback.
- A failing argument-spec check has `"failed": true` in its result, like every controller-side failure in Volant.


- A file that includes itself hits that limit and is reported as a task failure with exit code 2. Ansible recurses until the Python stack runs out, at exit code 250.
- An included file outside a role's `tasks/` and `handlers/` that does not parse fails the task, with exit code 2 and a recap. Ansible exits with code 4 and no recap. A failure the recap accounts for is more useful, and a `rescue` can catch it.
- A failing include inside a loop prints the usual `failed: [host] (item=...)` line. Ansible prints only an `[ERROR]` on standard error.
- `allow_duplicates: false` counts a role as run for a host when its tasks are brought in. ansible-core counts it when the host ran at least one of its tasks, so a role whose tasks all skip still runs again there.
- `allow_duplicates: false` on an `import_role` compares the statements while the play compiles, and so a `when` on the earlier statement does not change the result for a host.
