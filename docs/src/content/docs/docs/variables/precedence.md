---
title: Precedence and facts
description: Which source wins when two define the same variable, the magic variables, and how facts are gathered, named and cached.
---

This page answers two questions: which value a variable has when several sources set it, and where the facts about a host come from.

When two sources define the same variable, the one further down this list wins. The order is ansible-core 2.19's, with each role layer measured against its neighbors.

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
12. `include_vars`
13. `set_fact` and registered results
14. Role parameters, and the `vars:` of a dynamic include statement
15. `--extra-vars`

[Roles](/docs/playbooks/roles/#role-variables) explains where the three role layers come from and how they differ.

A vars file that holds only `---` and comments loads as an empty mapping, as in ansible-core. A file holding a scalar such as `42` is still refused.

A mapping can take the keys of another with a merge key, `<<: *base`, in every YAML file Volant reads: variable files, inventories, playbooks and task arguments. The merged keys come first, and a key written next to `<<` replaces the merged one. In `<<: [*a, *b]` the first mapping wins. A value that is not a mapping or a list of mappings stops the load, with the message of PyYAML.

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
| `ansible_play_batch`, `play_hosts` | The hosts of the current [`serial` batch](/docs/playbooks/how-a-play-runs/#serial-batches). `play_hosts` is deprecated. |
| `playbook_dir`, `inventory_dir`, `inventory_file` | Paths of the current run. |
| `omit` | The placeholder that drops a module argument. |
| `vars` | Every variable of this host, read by name. `vars['a']`, `vars[name]` and `vars.get('a')` render a template the variable holds, as `{{ a }}` does. `omit` is not in it. |
| `ansible_playbook_python` | The Python interpreter that runs ansible-core for Volant. Found when a template reads it, and undefined when none is found. |
| `ansible_check_mode`, `ansible_diff_mode` | Whether the run has `--check` and `--diff`. A play's or a task's own `check_mode` or `diff` keyword does not change them. |
| `ansible_run_tags`, `ansible_skip_tags` | The tags the run selects and skips, see [Check mode, diff and listings](/docs/playbooks/check-and-diff/#ansible_run_tags-and-ansible_skip_tags). |
| `ansible_forks` | The current `forks` value. |
| `ansible_verbosity` | How many `-v` the run was given, as a number. |
| `ansible_inventory_sources` | Every inventory source, in the order given, with paths made absolute. |
| `ansible_config_file` | The `ansible.cfg` the run read, as an absolute path, or `None` when it read none. |
| `ansible_host` | In a task, the inventory name of the host when nothing sets it, so `lookup('vars', 'ansible_host')` answers. `hostvars` does not get this default. |
| `ansible_version` | The ansible-core release Volant reproduces, see [decision record 0003](/docs/decisions/0003-ansible-version-reports-the-reference-release/). |
| `volant_version` | Volant's own version. Test `volant_version is defined` to tell the two engines apart. |

Inside a `rescue`, `ansible_failed_task` and `ansible_failed_result` are set as facts on the host that failed.

Every task also gets `ansible_search_path`, and a role's task gets `role_path`. Both are in place before the task's own `vars:` resolve, so a task variable built from `lookup('template')` finds the role's `templates/`. [Templating](/docs/variables/templating/#what-is-available) lists what the search path holds.

## hostvars

`hostvars` holds what the inventory, `--extra-vars`, `set_fact` and gathered facts produced for each host, with the host's magic variables (`group_names`, `inventory_dir`, `playbook_dir` and the run's options), a host `add_host` made included. It does not include another host's play or task variables. It is rebuilt whenever a fact changes.

Every host reads one shared map rather than its own copy, so a task that names `hostvars` costs a lookup, not a copy of the inventory.

A task that reads `hostvars`, `ansible_play_hosts` or `ansible_play_batch` waits for every host to reach it first, so it sees what Ansible would show at that point.

## Facts

Facts are what a host reports about itself, such as its name, distribution and addresses.

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Gathered facts rank above inventory variables and below the play's own variables, as the list at the top of this page shows.

### When facts are gathered

A play gathers facts unless it sets `gather_facts: false`. With `gathering = explicit` in `ansible.cfg`, only a play that writes `gather_facts` gathers. With `smart`, a host that has fresh facts in the [fact cache](/docs/hosts/inventories/#the-fact-cache) is skipped. A `gather_facts` template renders once, when the play starts.

Volant either runs ansible-core's own `setup` module through the [warm Python path](/docs/internals/python/), or lets the agent (the small program Volant puts on each host) collect the facts itself, when the playbook provably reads nothing else. See [Native facts](#native-facts).

Because `setup` can always fall back to its Python module, the controller needs ansible-core and the host needs a Python 3 interpreter. A run that needs Python only to gather facts does not install the [managed runtime](/docs/reference/runtime/), so a controller with no usable ansible-core stops a play that gathers facts before the first connection, instead of carrying on without them. Set `gather_facts: false` on plays that do not need facts.

`gather_subset` and `gather_timeout` work on a play, and on the `setup` module with `filter`, as in ansible-core.

### Two names for each fact

Each fact is readable in two places:

- flat, with an `ansible_` prefix: `ansible_hostname`
- inside `ansible_facts`, spelled the way the module returned it: `ansible_facts.hostname`

They are separate variables. A later `set_fact` of `ansible_hostname` shadows the flat one and leaves `ansible_facts.hostname` alone. Ansible behaves the same way.

### Native facts

A native fact collector is a version written in Rust that runs inside the agent, so the host needs no Python for it. It exists for Debian and Ubuntu. It collects the 17 collectors ansible-core runs for `gather_subset: min`, the processor and memory facts of ansible-core's `hardware` collector, and the default routes and address lists of its `network` collector. A fact the native collector does not produce, such as mounts, devices or per-interface details, is left out.

An absent fact only matters if something reads it. So before the first connection, Volant reads every play, role, template and variable definition of the run and lists the facts they can read. It uses the native collector only if all of them are facts it produces. A read by a computed name, such as `ansible_facts[item]`, `vars['ansible_' ~ name]`, `lookup('vars', ...)` or a whole `hostvars[host]` passed to a filter, could reach any fact, so it keeps ansible-core's `setup`.

The native collector also hands the task back on a host it cannot reproduce: it gives up and the Python `setup` module runs. That happens on another distribution, with SELinux enabled, with local facts in `/etc/ansible/facts.d`, or with a host name only DNS knows.

`--facts python` always runs ansible-core's module, and `--facts native` always asks the agent. `--profile` prints which one the run chose and why. See [Command line](/docs/reference/cli/#run-and-output).

### Facts are data

Facts are the host's own words, so they arrive untrusted: a template inside a fact stays text and is never rendered. See [Values from hosts are never templated](/docs/variables/templating/#values-from-hosts-are-never-templated).

### Keeping facts between runs

With `fact_caching = jsonfile` facts persist from one run to the next, see [The fact cache](/docs/hosts/inventories/#the-fact-cache).
