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

`import_playbook` splices the other file's plays where the statement stands, and each imported play keeps the directory of the file it came from. Its file name renders with no variables at all, so `{{ 'sub' }}.yml` works and `{{ nosuchvar }}.yml` does not.

## Dynamic includes

Of what the statement itself says, only its `vars:` travel into the included file. Those variables enter at the role-parameter level of the [precedence order](/variables/precedence/), above facts and below `--extra-vars`, and they accumulate through nested includes. The statement's own `tags` stop at the statement, and its `when` is evaluated for the statement alone.

What the layers around the statement say does reach the included tasks: the play, the role entry that brought the file in, and the blocks around it. Their `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` and `vars` apply to every included task, and a task's own value wins over an inherited one. This is ansible-core 2.19's rule, which skips a dynamically loaded parent and asks the grandparent. An inherited `check_mode: true` is refused in the included file rather than dropped.

An included task lands in the block and section where the statement was written, so a failing include inside a block body is caught by that block's `rescue`.

Volant checks what an include brings in before it runs. By then the hosts are connected and earlier tasks have run, so a module or keyword this release does not support yet can no longer stop the whole run. It fails the include statement for the host that reached it instead, nothing in the file runs, and a `rescue` around the statement can catch it.

The Python modules an included file names are found earlier. The union of Python modules is built before the first connection from every task and handler file of the roles in play, so a module named only in `setup-{{ ansible_os_family }}.yml` is already on the host when the include runs. A file under a role's `tasks/` or `handlers/` that does not parse stops the run before anything connects. See [The warm Python path](/internals/python/#how-it-works).

A `notify` in an included file that names no handler ends the run with exit code 1 and the pre-flight's sentence, as ansible-core does. No `ignore_errors` or `rescue` can catch it. See [Handlers](/playbooks/handlers/#differences-from-ansible).

## include_vars

`include_vars` produces variables rather than steps, so it is neither static nor dynamic in this sense. It is an ordinary controller-side module, and what it reads lands at facts precedence.

## Nesting limit

A statement 32 levels deep is rejected. One counter covers role nesting, `import_role` inside a role's tasks and `import_tasks` inside an imported file, because a cycle can run through any mix of the three.

## Differences from Ansible

- A file that includes itself hits that limit and is reported as a task failure with exit code 2. Ansible recurses until the Python stack runs out, at exit code 250.
- An included file outside a role's `tasks/` and `handlers/` that does not parse fails the task, with exit code 2 and a recap. Ansible exits with code 4 and no recap. A failure the recap accounts for is more useful, and a `rescue` can catch it.
- A failing include inside a loop prints the usual `failed: [host] (item=...)` line. Ansible prints only an `[ERROR]` on standard error.
- `include_role` does not support `allow_duplicates`, `rolespec_validate` and `apply` yet, and accepts `public: false` only, which is what Volant does. Honoring `public: true` would mean changing exported variables while the play's hosts stand at different steps.
- `include_vars` does not support `dir` and its six related options yet. A run that silently loaded none of a directory's variables would apply a playbook with defaults nobody wrote.
- `include_vars: file=x.yml name=ns` is read as one raw argument rather than split into `key=value` pairs, the same limitation `import_tasks` has.
- What `include_vars` produces sits one level above where Ansible puts it. You only notice with a name that a host variable also sets.
- A failing `include_vars` has `"failed": true` in its result.
