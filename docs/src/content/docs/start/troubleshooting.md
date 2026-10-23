---
title: When a run fails
description: Find the task that failed and why, check the same playbook under ansible-playbook, and report what you found.
---

Start from the exit code. It tells you whether the run stopped before it touched a host or failed on one.

| Code | What happened | Where to look |
|---|---|---|
| 4, nothing ran | The playbook stopped before the first connection. It does not parse, or it uses something this release does not support yet. | The message above the code. See [Stopped before the run](#stopped-before-the-run). |
| 2, 4 or 6 after a recap | A host failed (2), a host was unreachable (4), or both (6). | The `fatal:` line of the task, then [Failed during the run](#failed-during-the-run). |
| 1 | A role, a playbook or a host pattern was not found. | The file or the name the message quotes. |

[Exit codes](/reference/exit-codes/) lists every code.

## Stopped before the run

Most of what this release cannot run is named before Volant connects to anything. The message says what it refused and where:

| Message | What to do |
|---|---|
| `task 'NAME': keyword 'KEYWORD' is not supported yet on 'MODULE'` | Check the keyword in [Keywords](/reference/keywords/). Remove it, or keep this play under `ansible-playbook` for now. |
| `task 'NAME': argument 'ARGUMENT' is not supported yet on 'MODULE'` | Check the module in [Modules](/reference/modules/). Most refused arguments have a workaround there. |
| `couldn't resolve module/action 'NAME'` | Check the spelling. If the module comes from a collection, install it with `volant runtime install --collections NAMESPACE.COLLECTION`. |
| `strategy 'NAME' is not supported yet` | Use `linear`, `free`, `host_pinned` or `debug`. |
| `become_method 'NAME' is not supported yet` | Use `sudo` or `su`. This one exits with 2. |
| `the role 'NAME' was not found in ...` | The message lists the directories it searched. Add the role's directory to `roles_path`. This one exits with 1. |

[What is not supported yet](/playbooks/preflight/) explains the check, and [Will my playbook run?](/start/compatibility/) shows how to check a whole project without running it.

## Failed during the run

1. Run again on the failing host only, with more output. `-vvv` prints the arguments each module received:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 -vvv
   ```

2. Skip the tasks that already pass. `--start-at-task` starts at the task you name, and `--step` asks before each task:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 --start-at-task "Install nginx"
   ```

3. Stop at the failure and look around. With `debugger: on_failed` on the play or the task, Volant opens Ansible's task debugger when a task fails. You can print variables, change the arguments and run the task again.

## Compare with ansible-playbook

Volant reads the same files, so the same command runs under Ansible:

```sh
ansible-playbook -i inventory.ini site.yml --limit web1
```

If Ansible fails the same way, the problem is in the playbook or on the host. If only Volant fails, or the two give different results, that is a bug in Volant. You can keep the play under `ansible-playbook` while it is fixed.

## Report it

- A difference from `ansible-playbook` goes in an [Ansible compatibility gap](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) report.
- Anything else that does not work as documented goes in a [bug report](https://github.com/lorica-labs/volant/issues/new?template=bug.yml).
- A security problem goes through the [security policy](https://github.com/lorica-labs/volant/blob/main/SECURITY.md), never a public issue.

Include the output of `volant --version`, the smallest playbook and inventory that still fail, and the output of the failing run with `-vvv`. Remove passwords, keys, vault content and host names before you post them. A public issue stays public.
