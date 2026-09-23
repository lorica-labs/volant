---
title: Variable precedence
description: Where variables come from, which source wins, and the magic variables Volant adds.
---

When two sources define the same variable, the one further down this list wins. The order is ansible-core 2.19's, and each role layer was measured against its neighbors rather than taken from Ansible's documented numbering.

1. A role's `defaults/main.yml`
2. `group_vars/all`, from the inventory directory then the playbook directory
3. Inventory group variables, applied by group depth then name
4. `group_vars/<group>`, from the inventory directory then the playbook directory
5. Inventory host variables
6. `host_vars/<host>`, from the inventory directory then the playbook directory
7. Facts gathered from the host
8. Play `vars`
9. Play `vars_files`
10. A role's `vars/main.yml`
11. Task `vars`
12. `set_fact`, registered results and `include_vars`
13. Role parameters, and the `vars:` of a dynamic include statement
14. `--extra-vars`

[Roles](/playbooks/roles/#role-variables) explains where the three role layers come from and how they differ.

:::note
A gathered fact loses to the play's own variables. A playbook that sets a variable with the same name as a fact reads its own value, as in Ansible.
:::

## Magic variables

A fixed set of magic variables sits on top of every host's view and always wins:

| Variable | Holds |
|---|---|
| `inventory_hostname`, `inventory_hostname_short` | This host's inventory name. |
| `group_names`, `groups` | This host's groups, and every group with its hosts. |
| `hostvars` | Every host's variables, see below. |
| `ansible_play_hosts`, `ansible_play_hosts_all` | The play's live hosts, and all of its hosts. |
| `ansible_play_batch`, `play_hosts` | The hosts of the current [`serial` batch](/playbooks/serial/). `play_hosts` is deprecated. |
| `playbook_dir`, `inventory_dir`, `inventory_file` | Paths of the current run. |
| `omit` | The placeholder that drops a module argument. |
| `ansible_check_mode`, `ansible_diff_mode` | Always false in this release. |
| `ansible_forks` | The current `forks` value. |
| `ansible_version` | The reference release Volant reproduces, see [decision record 0003](/decisions/0003-ansible-version-reports-the-reference-release/). |
| `volant_version` | Volant's own version. Test `volant_version is defined` to tell the two engines apart. |

Inside a `rescue`, `ansible_failed_task` and `ansible_failed_result` are set as facts on the host that failed.

## hostvars

`hostvars` holds what the inventory, `--extra-vars`, `set_fact` and gathered facts produced for each host. It does not include another host's play or task variables. It is rebuilt whenever a fact changes.

Every host reads one shared map rather than its own copy, so a task that names `hostvars` costs a lookup, not a copy of the inventory.

A task that reads `hostvars`, `ansible_play_hosts` or `ansible_play_batch` waits for every host to reach it first, so it sees what Ansible would show at that point.
