---
title: When a run fails
description: Find the task that failed and why, check the same playbook under ansible-playbook, and report what you found.
---

Start from the exit code. It tells you whether the run stopped before it touched a host or failed on one.

| Code | What happened | Where to look |
|---|---|---|
| 4, with no recap | The playbook stopped before the first connection. It does not parse, or it uses something this release does not support yet. | The message above the code. See [Stopped before the run](#stopped-before-the-run). |
| 2, 4 or 6, with a recap | A host failed (2), a host was unreachable (4), or both (6). | The `fatal:` line of the task, then [Failed during the run](#failed-during-the-run). |
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

[Will my playbook run?](/start/compatibility/#what-it-refuses-today) lists these refusals and shows how to check a whole project without running it. [The pre-flight check](/internals/preflight/) explains how the check works.

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

## Go back to ansible-playbook

The same files run under `ansible-playbook` unchanged. Volant adds nothing to your playbooks, roles, inventories or `ansible.cfg`, so there is nothing to convert back.

On the hosts, the only thing left is the cached agent: a directory called `volant-agent-<version>-<uid>` in `~/.ansible/tmp`, and a `volant-blobs-<version>-<uid>` directory beside it if a run used Python modules. To remove them, delete them on each host, as the account the run used:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-*'
```

A host where tasks ran with `become` holds a second copy in the home directory of the account it escalated to. Run the same command as that account. To remove Volant from the controller as well, see [Uninstall](/start/installation/#uninstall). [What Volant leaves on your machines](/#what-volant-leaves-on-your-machines) lists everything.

## Report it

- A difference from `ansible-playbook` goes in an [Ansible compatibility gap](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) report.
- Anything else that does not work as documented goes in a [bug report](https://github.com/lorica-labs/volant/issues/new?template=bug.yml).
- A security problem goes through the [security policy](https://github.com/lorica-labs/volant/blob/main/SECURITY.md), never a public issue.

Include the output of `volant --version`, the smallest playbook and inventory that still fail, and the output of the failing run with `-vvv`. Remove passwords, keys, vault content and host names before you post them. A public issue stays public.
