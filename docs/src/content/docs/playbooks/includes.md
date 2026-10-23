---
title: Includes and imports
description: Static imports, dynamic includes, include_vars, and when each one is checked.
---

Five statements bring work in from another file. They fall into two families:

| Statement | Kind | When Volant reads the file | What a listing shows |
|---|---|---|---|
| `import_tasks` | static | while the play compiles | every task inside |
| `import_role` | static | while the play compiles | every task inside |
| `import_playbook` | static | while the play compiles | every play inside |
| `include_tasks` | dynamic | when a host reaches the statement | the statement only |
| `include_role` | dynamic | when a host reaches the statement | the statement only |

A dynamic include is resolved per host, because its file name can depend on that host's variables. The new steps are spliced in right behind the statement while the play runs.

## Static imports

An `import_tasks` path is looked for next to the importing file first, then in the role's own `tasks/` directory.

`import_playbook` splices the other file's plays where the statement stands, and each imported play keeps the directory of the file it came from. Its file name renders with the `-e` variables and through the [Python helper](/variables/python-helper/), so `{{ which }}.yml` works with `-e which=sub.yml`, and so does a name built with `lookup('env', ...)` or a collection filter. Nothing has an inventory yet, so a name cannot depend on a host. A name that reads an undefined variable stops the load with exit 4: `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`.

Three keywords of the `import_playbook` entry reach every play of the file:

- `tags` are added to the tags of each imported play, so `--tags` and `--skip-tags` select its tasks and the listings show them.
- `vars` are added to the variables of each imported play and win over the play's own.
- `when` is put in front of the conditions of every task, role and block of each imported play, and in front of its fact gathering. It is not evaluated once for the whole import. A false condition still opens each play and still prints a skip for each task and host. Handlers are not conditional.

The other keywords that ansible-core 2.19 accepts on the entry, such as `ignore_errors`, `become` and `environment`, do nothing there, and Volant ignores them the same way. Any other key is refused. A `tags` or a `when` that holds a template is refused too, because Volant has no variables when it loads the playbook.

## Dynamic includes

Of what the statement itself says, only its `vars:` travel into the included file. Those variables enter at the role-parameter level of the [precedence order](/variables/precedence/), above facts and below `--extra-vars`, and they accumulate through nested includes. The statement's own `tags` stop at the statement, and its `when` is evaluated for the statement alone.

What the layers around the statement say does reach the included tasks: the play, the role entry that brought the file in, and the blocks around it. Their `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` and `vars` apply to every included task, and a task's own value wins over an inherited one. This is ansible-core 2.19's rule, which skips a dynamically loaded parent and asks the grandparent. An inherited `check_mode: true` is refused in the included file rather than dropped.

### Putting keywords on what an include brings in

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

### Role options

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

An included task lands in the block and section where the statement was written, so a failing include inside a block body is caught by that block's `rescue`.

Volant checks what an include brings in before it runs. By then the hosts are connected and earlier tasks have run, so a module or keyword this release does not support yet can no longer stop the whole run. It fails the include statement for the host that reached it instead, nothing in the file runs, and a `rescue` around the statement can catch it.

The Python modules an included file names are found earlier. The union of Python modules is built before the first connection from every task and handler file of the roles in play, so a module named only in `setup-{{ ansible_os_family }}.yml` is already on the host when the include runs. A file under a role's `tasks/` or `handlers/` that does not parse stops the run before anything connects, for `roles:` and `import_role`. For an `include_role` the pre-run scan does not read the role: earlier plays and tasks run, then the run ends with exit 4 after the statement's banner, and a `when: false` on the include hides the failure. See [The warm Python path](/internals/python/#how-it-works).

A `notify` in an included file that names no handler ends the run with exit code 1 and the pre-flight's sentence, as ansible-core does. No `ignore_errors` or `rescue` can catch it. See [Handlers](/playbooks/handlers/#differences-from-ansible).

## include_vars

`include_vars` produces variables rather than steps, so it is neither static nor dynamic in this sense. It is an ordinary controller-side module, and what it reads lands at facts precedence.

`include_vars` reads one file, named by `file:` or written as the free-form path, or every file of a directory, named by `dir:`. The two forms take different options, and giving options of both forms fails the task.

A file is looked for in the places `include_vars` searches: `vars/` and then the directory itself, for a role and then for the playbook. A `dir:` is not searched for. A task of a role reads it under the role's `vars/`, and `dir: vars/x` names a directory of the role itself when that exists. Any other task reads it against the directory of the file that holds the task. An absolute path is used as it is.

A directory is read in a fixed order: directories sorted by their full path as text, then the files of each directory sorted by name. Hidden files and directories are read, and links to directories are followed. A later file replaces the keys of an earlier one whole. A file with nothing in it adds nothing, and it is still listed in `ansible_included_var_files`.

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

The result holds `ansible_facts` and `ansible_included_var_files`, the files in the order they were read. A task that reads a vaulted file is hidden like a `no_log` task: `-v` shows the censored line, and a loop item that read one shows `(censored due to no_log)`. The task runs in check mode, as it does in Ansible.

A missing directory, a path that is not a directory, a file without a valid extension and a file that is not a mapping fail the task with `message` set, as in Ansible. A failure in a directory keeps the variables of the directories before the failing one in the result, but a failed task sets no variables. A file that cannot be parsed or decrypted raises the error instead, and the task has no result of its own. An option of the wrong type fails with Python's own sentence.

## Nesting limit

A statement 32 levels deep is rejected. One counter covers role nesting, `import_role` inside a role's tasks and `import_tasks` inside an imported file, because a cycle can run through any mix of the three.

## Differences from Ansible

- A file that includes itself hits that limit and is reported as a task failure with exit code 2. Ansible recurses until the Python stack runs out, at exit code 250.
- An included file outside a role's `tasks/` and `handlers/` that does not parse fails the task, with exit code 2 and a recap. Ansible exits with code 4 and no recap. A failure the recap accounts for is more useful, and a `rescue` can catch it.
- A failing include inside a loop prints the usual `failed: [host] (item=...)` line. Ansible prints only an `[ERROR]` on standard error.
- `allow_duplicates: false` counts a role as run for a host when its tasks are brought in. The reference counts it when the host ran at least one of its tasks, so a role whose tasks all skip still runs again there.
- `allow_duplicates: false` on an `import_role` compares the statements while the play compiles, and so a `when` on the earlier statement does not change the result for a host.
- `include_vars: file=x.yml name=ns` is read as one raw argument rather than split into `key=value` pairs, the same limitation `import_tasks` has.
- What `include_vars` produces sits one level above where Ansible puts it. You only notice with a name that a `set_fact` also wrote: Ansible lets the `set_fact` win in either order, and here the later write wins.
