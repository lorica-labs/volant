# Variables and templating

## Sources, lowest precedence first

1. `group_vars/all` (inventory directory, then playbook directory)
2. Inventory group variables, applied by group depth and name
3. `group_vars/<group>` (inventory directory, then playbook directory)
4. Inventory host variables
5. `host_vars/<host>` (inventory directory, then playbook directory)
6. Play `vars`
7. Play `vars_files`
8. Task `vars`
9. Facts set by `register` and `set_fact`
10. `--extra-vars`

A fixed set of magic variables is added on top of every host's view and always wins: `inventory_hostname`, `inventory_hostname_short`, `group_names`, `groups`, `hostvars`, `play_hosts` and its aliases, `playbook_dir`, `inventory_dir`, `inventory_file`, `omit`, `ansible_check_mode`, `ansible_diff_mode`, `ansible_forks`, `ansible_version` and `volant_version` (see [ADR 0003](https://github.com/lorica-labs/volant/blob/main/docs/adr/0003-ansible-version-reports-the-reference-release.md)).

`hostvars` holds what the inventory and `set_fact` produced for each host, without the other host's own play or task variables, and it is rebuilt whenever a fact changes. No facts are gathered from remote hosts yet, so that is all a cross-host lookup can see.

## Task keywords

Play: `name`, `hosts`, `gather_facts`, `vars`, `vars_files`, `tasks`, `become`, `become_user`, `become_method`.

Task: `name`, `args`, `vars`, `when`, `loop`, `with_items`, `loop_control`, `register`, `changed_when`, `failed_when`, `timeout`, `ignore_errors`, `become`, `become_user`, `become_method`.

`when`, `changed_when` and `failed_when` take one Jinja2 expression or a list of them, all of which must hold. `loop` and `with_items` cannot both be given on the same task; `with_items` flattens one level of nested lists, `loop` does not. Registering a looped task collects a `results` list, one entry per item, the way `ansible-playbook` does.

Any other keyword is refused by name rather than ignored.

## Templating

Jinja2 templates render in strict mode: an undefined variable is an error, not an empty string, unless guarded with `default` or `is defined`. A non-boolean `when` is also refused.

On top of MiniJinja's own Jinja2 builtins, Volant adds Ansible's:

- **Filters**: `default`/`d`, `bool`, `int`, `float`, `mandatory`, `ternary`, `combine`, `dict2items`, `items2dict`, `to_json`, `to_nice_json`, `from_json`, `basename`, `dirname`, `split`, `regex_replace`, `regex_search`, `regex_findall`.
- **Tests**: `truthy`, `falsy`, `match`, `search`, `regex`, `contains`.
- **Lookups**: `env`, `file`, `vars`, `pipe`, under both their short name and their `ansible.builtin.*` form.

A filter, test or lookup outside this list fails by its own name, not silently. Every one of them is checked against `ansible-core` in the golden corpus (`crates/volant/tests/golden`).

For the modules whose arguments these variables and templates feed, see the [native module table](modules.md).

## Not there yet

- `!vault` and `!unsafe` YAML tags: detected and refused by name; no decryption or unsafe marking.
- Roles, collections beyond native modules, handlers and `notify`.
- `serial`, `run_once`, `delegate_to`.
- `until`/`retries`, `block`/`rescue`.
- `include_*` and `import_*`.
- `gather_facts` is accepted but does nothing: Volant warns and continues without facts. The `setup` module does not exist yet, so no `ansible_*` fact beyond the magic variables above is ever defined.
- Filters, tests and lookups Ansible has beyond the list above, including `to_yaml`, `b64encode`, `hash`, `password_hash`, `ipaddr`, `version` and `json_query`: refused by name until a role in the compatibility target needs one.
- `hostvars` is copied into every host's variables on every task, so the cost of resolving variables grows with the square of the inventory size. The map is cached between facts, which is enough for a few hundred hosts: on a debug build over twenty local tasks, 50 hosts take 0.25 s, 100 hosts 0.76 s and 200 hosts 2.66 s.

For the connection settings, the agent cache, `become` and the host-pattern grammar, see [Connections and privilege escalation](connections.md).
