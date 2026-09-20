# Variables and templating

## Sources, lowest precedence first

1. A role's `defaults/main.yml`
2. `group_vars/all` (inventory directory, then playbook directory)
3. Inventory group variables, applied by group depth and name
4. `group_vars/<group>` (inventory directory, then playbook directory)
5. Inventory host variables
6. `host_vars/<host>` (inventory directory, then playbook directory)
7. Play `vars`
8. Play `vars_files`
9. A role's `vars/main.yml`
10. Task `vars`
11. Facts set by `register` and `set_fact`
12. A role's parameters (a free key on a role entry)
13. `--extra-vars`

See [Roles](playbooks.md#roles) for where the three role layers come from and how they differ
from each other.

A fixed set of magic variables is added on top of every host's view and always wins: `inventory_hostname`, `inventory_hostname_short`, `group_names`, `groups`, `hostvars`, `ansible_play_hosts`, `ansible_play_hosts_all`, `ansible_play_batch`, the deprecated `play_hosts`, `playbook_dir`, `inventory_dir`, `inventory_file`, `omit`, `ansible_check_mode`, `ansible_diff_mode`, `ansible_forks`, `ansible_version` and `volant_version` (see [ADR 0003](https://github.com/lorica-labs/volant/blob/main/docs/adr/0003-ansible-version-reports-the-reference-release.md)).

`ansible_play_hosts` and `ansible_play_hosts_all` name the play, while `ansible_play_batch` and `play_hosts` name the current [`serial` batch](playbooks.md#serial), which is what the reference reports there. Inside a `rescue`, `ansible_failed_task` and `ansible_failed_result` are set as facts on the host that failed.

`hostvars` holds what the inventory, `--extra-vars` and `set_fact` produced for each host, without the other host's own play or task variables, and it is rebuilt whenever a fact changes. No facts are gathered from remote hosts yet, so that is all a cross-host lookup can see. Every host of a run reads one shared map rather than a copy of its own, so a task naming `hostvars` costs a lookup and not a copy of the inventory.

## Conditions and loops

`when`, `changed_when`, `failed_when` and `until` take one Jinja2 expression or a list of them, all of which must hold. `loop` and `with_items` cannot both be given on the same task; `with_items` flattens one level of nested lists, `loop` does not. Registering a looped task collects a `results` list, one entry per item, the way `ansible-playbook` does.

Under `loop_control`, only `loop_var` and `label` are read. The rest are refused by name, and a
sub-key `ansible-core` does not have refuses the playbook outright.

Which keywords this release runs and which it refuses is in [Keywords](keywords.md), a page
generated from the tables the loader and the pre-flight read, so it cannot describe a release
other than this one. What each of them does is in [Playbooks](playbooks.md).

## Templating

Jinja2 templates render in strict mode: an undefined variable is an error, not an empty string, unless guarded with `default` or `is defined`. A non-boolean `when` is also refused.

On top of MiniJinja's own Jinja2 builtins, Volant adds Ansible's:

- **Filters**: `default`/`d`, `bool`, `int`, `float`, `mandatory`, `ternary`, `combine`, `dict2items`, `items2dict`, `to_json`, `to_nice_json`, `from_json`, `basename`, `dirname`, `split`, `regex_replace`, `regex_search`, `regex_findall`.
- **Tests**: `truthy`, `falsy`, `match`, `search`, `regex`, `contains`.
- **Lookups**: `env`, `file`, `vars`, `pipe`, under both their short name and their `ansible.builtin.*` form.

A filter, test or lookup outside this list fails by its own name, not silently. Every one of them is checked against `ansible-core` in the golden corpus (`crates/volant/tests/golden`).

## Trusted and untrusted values

A template you wrote is code, and a value a managed host sent back is data. Volant keeps the two apart the way ansible-core 2.19 does, so a string a host controls never runs as a template on the controller.

A variable is untrusted when it came from a host or from a file read during the run: `register`, a module result, a `set_fact`, and `lookup('file')`, `lookup('pipe')` or `lookup('env')`. Everything the playbook itself carries stays trusted, including play and task `vars`, `--extra-vars`, inventory variables, `vars_files` and `include_vars`.

Two things follow, and both are changes from 0.1.0-alpha.5:

- A value that is untrusted renders once. If what comes out looks like a template again, it is left as text rather than rendered a second time, which is what the reference does.
- `debug: var:` names an expression to evaluate. Given a name that came from a host, the task fails with the reference's own sentence: ``Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.`` Writing the name yourself, as in `debug: var: result.stdout`, is unaffected; it is `debug: var: "{{ from_a_host }}"` that stops.

The grain is the variable, not the string. Ansible tags each string object, so a trusted string survives being passed through a template and loses its trust the moment an expression builds a new one from it. Here a value is untrusted when the render that produced it read something a host contributed, which is the same answer wherever a host took part, and more permissive on a value the playbook computed out of its own text.

For the modules whose arguments these variables and templates feed, see the [native module table](modules.md).

## Not there yet

- `!vault` and `!unsafe` YAML tags: detected and refused by name; no decryption or unsafe marking.
- Collections. Roles load from the standard search paths; a collection does not.
- `gather_facts` is accepted but gathers nothing: a play that asks for facts is warned and carries on without them, and a play that writes `false` runs as it does in Ansible. The `setup` module does not exist yet, so no `ansible_*` fact beyond the magic variables above is ever defined.
- Filters, tests and lookups Ansible has beyond the list above, including `to_yaml`, `b64encode`, `hash`, `password_hash`, `ipaddr`, `version` and `json_query`: refused by name until a role in the compatibility target needs one.
- Methods on a mapping. `{{ hostvars.keys() }}` renders in Ansible and fails here, because the templating engine underneath has no `keys` on a map yet. `dict2items` is the way round it.
- Resolving variables costs more than linearly in the size of the inventory, and two rounds of sharing have taken most of that cost out. Sharing `hostvars` came first: a task reads one host's entry out of a map every host shares, instead of copying every host's variables, which took 56% off the part of the cost that grows with the square of the inventory. Sharing the inventory-wide magic variables came next: `groups` and the play's live host lists are built once per batch and read from there, rather than written into every host's variables for every task. That took another 31% off the same part and 43% off the part that grows with the inventory alone, for a 200-host run about 1.5 times faster. What is left of the bend has not been measured; the next suspect is the bookkeeping each host copies at every step. Nothing in a playbook, an inventory or `ansible.cfg` changes any of this — it is the engine's own cost, not something a run can be written around. Measured on a debug build over twenty local tasks, after 0.1.0-alpha.5: 50 hosts 0.16 s, 100 hosts 0.50 s, 200 hosts 1.45 s. Read those as the shape of the curve rather than as timings you should see.

For the connection settings, the agent cache, `become` and the host-pattern grammar, see [Connections and privilege escalation](connections.md).
