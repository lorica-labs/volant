# Keywords

Every play, block and task keyword ansible-core 2.19 knows, and what this release does with each one.

A playbook is loaded against the whole grammar, so it parses here as it parses there. What this release cannot execute is refused by name, rather than accepted and then ignored. A keyword the playbook writes is refused before the first connection. A file that a dynamic `include_tasks` or `include_role` names is read when a host reaches the statement, so a keyword written there is refused at that moment instead: the statement fails for the host that asked, a `rescue` around it can take that failure, and nothing in the file runs. What `import_tasks` and `import_role` name is compiled with the play and checked with it.

A keyword carries a status per place it can be written:

- `runs`: this release honours it, or refuses by name the one value it cannot do.
- `partial`: it is accepted and answered, but not with the whole of what the reference does with it. What is missing is spelled out under the table.
- `refused`: it loads, and the run stops before anything connects. In a file a dynamic include names, the statement fails when a host reaches it instead.
- `not accepted`: it cannot be written there, and a playbook that writes it there is refused when it is read, as the reference refuses it.

This page is generated from the tables in the source, so it cannot drift from them. Run `just docs-keywords` after changing a table.

| Keyword | Play | Block | Task |
|---|---|---|---|
| `action` | not accepted | not accepted | refused |
| `always` | not accepted | runs | not accepted |
| `any_errors_fatal` | refused | refused | refused |
| `args` | not accepted | not accepted | runs |
| `async` | not accepted | not accepted | refused |
| `become` | runs | runs | runs |
| `become_exe` | refused | refused | refused |
| `become_flags` | refused | refused | refused |
| `become_method` | runs | runs | runs |
| `become_user` | runs | runs | runs |
| `block` | not accepted | runs | not accepted |
| `changed_when` | not accepted | not accepted | runs |
| `check_mode` | partial | partial | partial |
| `collections` | refused | refused | refused |
| `connection` | refused | refused | refused |
| `debugger` | refused | refused | refused |
| `delay` | not accepted | not accepted | runs |
| `delegate_facts` | not accepted | runs | runs |
| `delegate_to` | not accepted | runs | runs |
| `diff` | refused | refused | refused |
| `environment` | runs | runs | runs |
| `fact_path` | refused | not accepted | not accepted |
| `failed_when` | not accepted | not accepted | runs |
| `force_handlers` | runs | not accepted | not accepted |
| `gather_facts` | runs | not accepted | not accepted |
| `gather_subset` | refused | not accepted | not accepted |
| `gather_timeout` | refused | not accepted | not accepted |
| `handlers` | runs | not accepted | not accepted |
| `hosts` | runs | not accepted | not accepted |
| `ignore_errors` | refused | runs | runs |
| `ignore_unreachable` | refused | refused | refused |
| `local_action` | not accepted | not accepted | refused |
| `loop` | not accepted | not accepted | runs |
| `loop_control` | not accepted | not accepted | runs |
| `max_fail_percentage` | refused | not accepted | not accepted |
| `module_defaults` | refused | refused | refused |
| `name` | runs | runs | runs |
| `no_log` | runs | runs | runs |
| `notify` | not accepted | runs | runs |
| `order` | refused | not accepted | not accepted |
| `poll` | not accepted | not accepted | refused |
| `port` | refused | refused | refused |
| `post_tasks` | runs | not accepted | not accepted |
| `pre_tasks` | runs | not accepted | not accepted |
| `register` | not accepted | not accepted | runs |
| `remote_user` | refused | refused | refused |
| `rescue` | not accepted | runs | not accepted |
| `retries` | not accepted | not accepted | runs |
| `roles` | runs | not accepted | not accepted |
| `run_once` | runs | runs | runs |
| `serial` | runs | not accepted | not accepted |
| `strategy` | runs | not accepted | not accepted |
| `tags` | runs | runs | runs |
| `tasks` | runs | not accepted | not accepted |
| `throttle` | refused | refused | refused |
| `timeout` | refused | runs | runs |
| `until` | not accepted | not accepted | runs |
| `vars` | runs | runs | runs |
| `vars_files` | runs | not accepted | not accepted |
| `vars_prompt` | refused | not accepted | not accepted |
| `when` | not accepted | runs | runs |
| `with_config` | not accepted | not accepted | refused |
| `with_csvfile` | not accepted | not accepted | refused |
| `with_dict` | not accepted | not accepted | refused |
| `with_env` | not accepted | not accepted | refused |
| `with_file` | not accepted | not accepted | refused |
| `with_fileglob` | not accepted | not accepted | refused |
| `with_first_found` | not accepted | not accepted | refused |
| `with_indexed_items` | not accepted | not accepted | refused |
| `with_ini` | not accepted | not accepted | refused |
| `with_inventory_hostnames` | not accepted | not accepted | refused |
| `with_items` | not accepted | not accepted | runs |
| `with_lines` | not accepted | not accepted | refused |
| `with_list` | not accepted | not accepted | refused |
| `with_nested` | not accepted | not accepted | refused |
| `with_password` | not accepted | not accepted | refused |
| `with_pipe` | not accepted | not accepted | refused |
| `with_random_choice` | not accepted | not accepted | refused |
| `with_sequence` | not accepted | not accepted | refused |
| `with_subelements` | not accepted | not accepted | refused |
| `with_template` | not accepted | not accepted | refused |
| `with_together` | not accepted | not accepted | refused |
| `with_unvault` | not accepted | not accepted | refused |
| `with_url` | not accepted | not accepted | refused |
| `with_varnames` | not accepted | not accepted | refused |
| `with_vars` | not accepted | not accepted | refused |

## What `partial` leaves out

A keyword below is accepted wherever the grid says `partial`, and answered the same way in each of those places.

| Keyword | What is missing |
|---|---|
| `check_mode` | only `false` is accepted, and it is honoured by running for real; `true` is refused by name, because this release has no check mode: before the first connection when the playbook itself writes it, and at the statement that read the file when a dynamic include brought it in |

## Handlers

A handler takes every task keyword above, and one of its own.

| Keyword | Status |
|---|---|
| `listen` | runs |

## Under `loop_control`

A sub-key ansible-core does not have is refused when the playbook is read.

| Sub-key | Status |
|---|---|
| `break_when` | refused |
| `extended` | refused |
| `extended_allitems` | refused |
| `index_var` | refused |
| `label` | runs |
| `loop_var` | runs |
| `pause` | refused |
