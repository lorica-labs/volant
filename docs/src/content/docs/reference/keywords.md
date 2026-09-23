---
title: Keywords
description: Every play, block and task keyword ansible-core 2.19 knows, and what Volant does with each one.
---

<!-- Generated from crates/volant/src/keywords.rs by `just docs-keywords`. Do not edit by hand. -->

Volant loads a playbook against the whole ansible-core 2.19 grammar, so it parses here as it parses under Ansible. What this release does not support yet is named before the run starts, never accepted and then ignored. Each keyword has a status for each place it can be written.

<ul class="kw-legend not-content">
<li><span class="kw kw-runs">runs</span><span>Volant honors it, or names the one value it does not support yet.</span></li>
<li><span class="kw kw-partial">partial</span><span>Accepted and honored, but not everything Ansible does with it. The gap is listed below the grid.</span></li>
<li><span class="kw kw-notyet">not yet</span><span>Not supported yet. The playbook loads, and the run stops before anything connects. In a file a dynamic include names, the statement fails when a host reaches it instead.</span></li>
<li><span class="kw kw-no">not accepted</span><span>Cannot be written there. Ansible rejects it too, when it reads the playbook.</span></li>
</ul>

A keyword inside a file that only a dynamic `include_tasks` or `include_role` names is checked when a host reaches that statement: if it is not supported yet, the statement fails for that host, a `rescue` around it can catch the failure, and nothing in the file runs. See [Includes and imports](/playbooks/includes/).

This page is generated from the tables the loader and the pre-flight read, so it always describes the release it ships with.

## All keywords

<div class="kw-grid-wrap not-content">
<table class="kw-grid">
<thead><tr><th>Keyword</th><th>Play</th><th>Block</th><th>Task</th></tr></thead>
<tbody>
<tr><td><code>action</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>always</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>any_errors_fatal</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>args</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>async</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>become</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>become_exe</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>become_flags</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>become_method</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>become_user</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>block</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>changed_when</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>check_mode</code></td><td><span class="kw kw-partial">partial</span></td><td><span class="kw kw-partial">partial</span></td><td><span class="kw kw-partial">partial</span></td></tr>
<tr><td><code>collections</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>connection</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>debugger</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>delay</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>delegate_facts</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>delegate_to</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>diff</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>environment</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>fact_path</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>failed_when</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>force_handlers</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>gather_facts</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>gather_subset</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>gather_timeout</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>handlers</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>hosts</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>ignore_errors</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>ignore_unreachable</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>local_action</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>loop</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>loop_control</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>max_fail_percentage</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>module_defaults</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>name</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>no_log</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>notify</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>order</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>poll</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>port</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>post_tasks</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>pre_tasks</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>register</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>remote_user</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>rescue</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>retries</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>roles</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>run_once</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>serial</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>strategy</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>tags</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>tasks</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>throttle</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>timeout</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>until</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>vars</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>vars_files</code></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>vars_prompt</code></td><td><span class="kw kw-notyet">not yet</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td></tr>
<tr><td><code>when</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>with_config</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_csvfile</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_dict</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_env</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_file</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_fileglob</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_first_found</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_indexed_items</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_ini</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_inventory_hostnames</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_items</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-runs">runs</span></td></tr>
<tr><td><code>with_lines</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_list</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_nested</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_password</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_pipe</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_random_choice</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_sequence</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_subelements</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_template</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_together</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_unvault</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_url</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_varnames</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
<tr><td><code>with_vars</code></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-no">not accepted</span></td><td><span class="kw kw-notyet">not yet</span></td></tr>
</tbody>
</table>
</div>

## What partial leaves out

Each keyword below behaves the same way in every place the grid marks it `partial`.

| Keyword | What is missing |
|---|---|
| `check_mode` | only `false` is accepted, and the task runs for real. `true` is not supported yet because this release has no check mode: the run stops before the first connection when the playbook writes it, and at the include statement when a dynamic include brings it in |

## Handlers

A handler accepts every task keyword above, plus one of its own.

| Keyword | Status |
|---|---|
| `listen` | <span class="kw kw-runs">runs</span> |

## Under loop_control

A sub-key ansible-core does not know is rejected when the playbook loads.

| Sub-key | Status |
|---|---|
| `break_when` | <span class="kw kw-notyet">not yet</span> |
| `extended` | <span class="kw kw-notyet">not yet</span> |
| `extended_allitems` | <span class="kw kw-notyet">not yet</span> |
| `index_var` | <span class="kw kw-notyet">not yet</span> |
| `label` | <span class="kw kw-runs">runs</span> |
| `loop_var` | <span class="kw kw-runs">runs</span> |
| `pause` | <span class="kw kw-notyet">not yet</span> |
