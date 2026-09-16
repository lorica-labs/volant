# Playbooks

How a play becomes a run, and what each keyword does once it gets there.
[Keywords](keywords.md) says which keywords this release executes and which it refuses;
this page says what executing one means. Each section ends with the places where Volant
deliberately does something other than `ansible-core 2.19.12`, and why.

## How a play is compiled

A play is written as a tree and run as a sequence, so Volant flattens it once, before it
connects anywhere. Every host then walks the same numbered list of steps, in the order the
reference runs them:

1. `pre_tasks`
2. a handler flush
3. the roles, each with its dependencies in front of it, then `tasks`
4. a handler flush
5. `post_tasks`
6. a handler flush

The three flush points are always laid out, because whether the play has a handler at all is
not known until the roles have been read from disk. A play with nothing to flush pays three
steps that report nothing and show nothing.

Keywords written on a play, a role entry or a block are folded into each task under them while
the play is compiled, with the innermost value winning. So a `become: true` on the play and a
`become: false` on one task give exactly what the reference gives, without the run having to
carry the tree around.

Blocks survive the flattening as a span plus the section each step sits in, and a small jump
table over those spans decides where a host goes after a step succeeds, after it fails and after
it is stepped over.

## Roles

A role is static composition: its directory is read while the play is being compiled, and its
tasks are spliced into the step list. Nothing about it waits for a host.

`roles:` entries, `meta/main.yml` dependencies and `import_role` all name a role the same way.
A role is looked for under `<playbook_dir>/roles`, then `roles_path` from `ansible.cfg` or
`ANSIBLE_ROLES_PATH`, then `<playbook_dir>` itself; with no `roles_path` set, the three default
directories in the middle are `~/.ansible/roles`, `/usr/share/ansible/roles` and
`/etc/ansible/roles`. A three-part name such as `acme.demo.hello` is also looked for under
`<collections path>/ansible_collections/acme/demo/roles/hello`. A role nobody can find stops the
run at exit 1, naming every path that was tried.

Each of a role's directories is read file before directory: `tasks/main.yml` if it is there,
otherwise `tasks/main/`. `tasks_from`, `vars_from`, `defaults_from` and `handlers_from` on the
entry pick a name other than `main`.

`meta/main.yml` contributes two things. Its `dependencies` are roles in their own right, run in
front of the role that asked for them, and a dependency two roles share runs once.
`allow_duplicates: true` turns the deduplication off for that role. What is deduplicated is not
the name alone: the entry's parameters, its `vars:`, its `when:` and its `*_from` selections are
all part of the identity, so `- base` written twice runs once while `- base` and
`- { role: base, p: v }` run twice.

`meta/argument_specs.yml` turns into a task of its own, after the role's dependencies and in
front of the role's first task, which checks the arguments the role was given against the spec
entry named after its tasks file. The task's name carries the role's `short_description` behind
a dash, so `--list-tasks` shows
`Validating arguments against arg spec 'main' - The spec role`, and it carries the `always` tag,
so no `--tags` drops it. It checks in the order the reference checks: a missing required
argument, then a type, then a value outside `choices`, then an argument the spec does not name.
Sub-options, `aliases`, `default` and `mutually_exclusive` are not checked yet.

Three layers come out of a role and they sit at different heights. `defaults/main.yml` is at the
bottom, under the inventory's own variables. `vars/main.yml` sits above the play's `vars:` and
under a task's. A free key on a role entry (`{ role: base, p: v }`) is a role parameter, sits
above facts, and beats a `set_fact` of the same name; `vars:` on that same entry does not, and
travels as an ordinary task keyword. The two spellings look alike and are not, which is worth
remembering when a role reads a value you did not expect. A role's `defaults` and `vars` are
lent to the whole play, so `pre_tasks` and the roles after it can read them; the role's own
values are laid back over that for its own steps. Its parameters do not leave it.

A role's `handlers/` directory is read with its tasks, and a role's handlers run before the
play's own at the same flush.

Where this differs from Ansible:

- A role name containing a `.` is tried as a path before it is tried as a collection role, which
  is what makes a role directory whose own name contains a dot reachable.
- `collections_path` from `ansible.cfg` and `ANSIBLE_COLLECTIONS_PATH` are read the way
  `roles_path` is. That shape was not measured against the reference.
- `import_role` refuses `public`, `allow_duplicates` and `rolespec_validate` by name. The
  reference honours them, and accepting a `rolespec_validate: false` this engine ignores would
  fail a run the reference passes.
- A missing `import_tasks` file is reported with the reference's own two sentences and without
  its third, which names a Python error number for a module that was never called.
- `import_playbook` written as a task is refused before the play instead of failing under a play
  banner. The exit code is the reference's 2.
- A file importing itself is refused at exit 1 with a sentence of its own. The reference
  recurses until the interpreter gives up, at exit 250 with a quarter of a megabyte of
  traceback. Matching that would promise a crash report this engine does not produce.
- A failing argument-spec check carries `"failed": true` in its result body, which is what every
  controller-side failure in this engine does.

## Blocks

A block groups tasks so that `rescue` can catch their failure and `always` can run whatever
happened. Keywords on the block reach every task inside it.

When a task fails, the host looks outward for the nearest enclosing block that has a non-empty
`rescue` and enters it. Inside that rescue, `ansible_failed_task` and `ansible_failed_result`
are set as facts on that host: the task as it was written, and the result that failed. A host
that recovers this way is counted `rescued` in the recap and does not leave the play, because
only a failure nobody rescued shrinks the set of live hosts.

`ansible_failed_task` and `ansible_failed_result` outlive the block and the play: a task written
after the block, and a task in the play behind it, still read them.

`always` runs whether the body succeeded, failed or was rescued, and it runs on the way out of a
failure too, innermost block first. A failure raised inside an `always` does not finish that
section: the rest of it is skipped, and the host carries on outward, running the `always` of
every block still enclosing it.

A failure raised inside a `rescue` is not caught by that block. It goes to the next rescue
outward, which is the grandparent's.

Three things are never rescued. A host that never entered a rescue steps over every step in it,
a flush point and the handlers behind it included. A host that cannot be reached leaves the play
rather than entering a rescue, since there is nothing left to run one on. And when a `run_once`
task fails, the hosts that were waiting on its runner leave the play even if a rescue keeps the
runner itself in.

Where this differs from Ansible:

- A keyword this release does not execute reads as `null` in `ansible_failed_task` rather than
  as the reference's default for it, because a default such as `connection: "ssh"` would
  describe something nothing here honours.
- `ansible_failed_task` carries one key per keyword and leaves out the six the reference keeps
  for itself (`uuid`, `finalized`, `squashed`, `_resolved_action`, `async_val`, `loop_with`).
  `args` holds what the playbook wrote, without the nineteen `_ansible_*` keys the reference
  adds, none of which exists here.
- A host whose `ansible_connection` names a transport this release does not have is reported
  unreachable, so a rescue does not catch it. The reference treats it as an ordinary task
  failure and rescues it.

## Handlers

A handler runs at a flush point, once, however many tasks notified it. The three automatic flush
points are listed above; `meta: flush_handlers` adds one wherever it is written, with the
`TASK [meta]` banner the reference prints there.

A flush point written inside an `include_tasks` runs only for the hosts whose own statement asked
for that file. The rest report the step, run no handlers there, and keep what they have notified
for the next flush they reach.

`notify` names a handler. The name reaches the first handler carrying it as its `name`, and
every handler listening for it through `listen`. The asymmetry is the reference's own, and it is
what makes a role run three times contribute one handler rather than three. Handlers run in the
order they were defined, not in the order they were notified, and a role named in `roles:` or by
`import_role` contributes its handlers in front of the play's own. A role brought in by
`include_role` is the other way round: its handlers go behind the play's, as in the reference.

A task notifies only when it reports `changed`; for a loop the aggregate decides, so one changed
item notifies. The notification list is emptied when the flush's handler steps are behind the
host, so a handler notified twice does not come back at the next flush.

A handler that fails takes the host out the way any other failing task does, and it stops the handlers
behind it in the same flush. A flush written inside a block is part of that block, so a handler
that fails there is caught by the block's `rescue`.

By default a host that has failed does not run the handlers it notified. `force_handlers` on the
play, `force_handlers` in `[defaults]`, or `--force-handlers` on the command line makes it run
them; the play keyword speaks over the other two, and the flag and the file behave alike. Under
it a failed host walks the rest of the play, reports every step and runs the handlers it
notified; the recap still counts the failure.

Handlers appear in no listing, and neither do the three automatic flush points.

Where this differs from Ansible:

- A `notify` naming a handler that does not exist is refused before the first connection, with
  the reference's own sentence and its exit code 1. The reference prints the notifying task's
  banner first, so the difference is that nothing has run when you read it.
- A `notify` whose value is a template is refused at exit 4. Resolving one per host and per loop
  item would mean failing a task that has already printed its result line.
- A block written in a `handlers:` list is refused. There is nowhere to put a block's `rescue`
  inside a flush.
- A `force_handlers` value that is neither true nor false leaves the setting alone, where the
  reference reads it as false.

## Tags and the listing commands

`tags` can be written on a play, a role entry, a block or a task, and the compiler folds the
outer tags into every task under them. `--tags` and `--skip-tags` then decide which tasks are
compiled in at all, so a tag settles what runs and what the listings show at the same time.

The algebra is the reference's own. A task carrying `always` survives every `--tags`, and
survives `--skip-tags all` unless `always` is itself skipped. A task carrying `never` is left
out of `--tags all` and of `--tags tagged`, and comes back only when something names it. A task
carrying no tag is not an empty set but the one-element set `untagged`, which is why
`--tags untagged` selects it. `[tags] run` and `[tags] skip` in `ansible.cfg`, and
`ANSIBLE_RUN_TAGS` and `ANSIBLE_SKIP_TAGS`, are defaults that the command line adds to rather
than replaces.

Tags written on an `include_tasks` or an `include_role` stop at the statement: `--tags inc` runs
the include, not what it brings in. The play's own tags do reach the included tasks. That is the
reference's own behaviour rather than a simplification made here.

Four commands read a playbook and print what running it would do without connecting to anything:
`--list-tasks`, `--list-tags`, `--list-hosts` and `--syntax-check`. Their layout is the
reference's down to the whitespace, and it is compared to the reference byte for byte over a
corpus of playbooks in the repository. The listings walk a block's body only, so a task written
in a `rescue:` or an `always:` appears in none of them, exactly as in the reference.

Where this differs from Ansible:

- A play with more than one tag has them printed sorted, and `--list-hosts` prints a play's hosts
  in inventory order. The reference joins a Python set in both places, so its order changes
  between runs and there is nothing to compare against.
- A listing resolves no module. A playbook whose modules this release cannot run still lists and
  still syntax-checks, so you can read a role before the release that runs its modules ships.
- A task whose `ignore_errors` or `become` is neither true nor false makes a listing exit 4,
  where the reference lists it and exits 0. That value is a malformed playbook rather than an
  unimplemented feature, and it is wrong in every release.

## `until`, `retries` and `delay`

`until` runs a task again until its expression holds. `retries` is the number of attempts in
all, not the number of extra ones, and it is three when only `until` is written; a `retries`
below 1 turns the machinery off, so the task runs once and carries no `attempts`. Volant waits
`delay` seconds, five by default, after every failed attempt including the last one, so three
attempts at the default delay cost fifteen seconds of waiting. The result carries the attempt
count under `attempts`, and a task that runs out of attempts has failed even if the module
itself passed. `retries` written without `until` runs the task again while its result is failed.
A looping task retries each item separately, and finishes one item's attempts before starting
the next.

Volant refuses all three on `include_tasks` and `include_role`, by name, before the first
connection: a statement it expands never becomes a task it could attempt twice. Ansible refuses
them too, at load time (`'until' is not a valid attribute for a TaskInclude`). `include_vars` is
an ordinary module on both sides and retries like any other task.

Where this differs from Ansible:

- A retried controller-side task prints the ordinary result body and the ordinary failure
  wording, where the reference has a shape of its own for those (`Action failed:`, a
  `Result was:` line per retry, a `retries` key in the result).
- An `until` expression that cannot be evaluated is reported with the reference's prefix and this
  engine's own sentence, as a failing `when` already is.
- `FAILED - RETRYING` for a task inside a role shows the task's own name rather than
  `role : name`.

## `serial`

`serial` cuts the play's hosts into batches, in inventory order, and plays each batch in turn
with its own banner, its own handler flushes and its own `run_once` election. A number is a
count of hosts; a string ending in `%` is a share, taken of the whole host list rather than of
what is left, truncated rather than rounded, and never less than one host. A list gives one size
per batch and its last element repeats once the list runs out. `serial: 0`, `serial: -1`,
`serial: []` and a `serial` larger than the inventory all mean one batch holding every host.

The run stops at the batch that had live hosts and lost every one of them, which is where the
reference stops it: exit 2 when they failed, exit 4 when they all went unreachable. A batch
whose hosts had already failed in an earlier play is not that, so it prints its banner and the
run carries on. Inside a batch, `ansible_play_batch` and the deprecated `play_hosts` name the
batch while `ansible_play_hosts` names the play, and `vars_files` is re-rendered and re-read
once per batch rather than once per play.

Where this differs from Ansible:

- A `serial` that is not a number (`serial: abc`, or `"50% "` with a trailing space) is refused
  at exit 4 with the reference's own sentence. The reference crashes at exit 250, which is it
  reporting a bug in itself rather than refusing a playbook.
- An undefined variable in `serial` is reported with the reference's prefix and this engine's
  sentence, at the measured exit 4.
- `play_hosts` holds what the reference holds, and no deprecation warning is printed for it.

## Includes

Five statements bring work in from elsewhere. `import_tasks`, `import_role` and
`import_playbook` are static: the file is read while the play is being compiled, and a listing
shows every task that came out of it. `include_tasks` and `include_role` are dynamic: what they
name is not known until the host that reaches them has rendered its own variables, so a listing
shows the statement and not its contents, and the steps are spliced in behind it while the play
runs.

An `import_tasks` path is looked for beside the importing file first, and in the role's own
`tasks/` directory second. `import_playbook` splices the other file's plays where the statement
stands rather than appending them, and each imported play keeps the directory of the file it
came from; its name renders with no variables at all, so `{{ 'sub' }}.yml` works and
`{{ nosuchvar }}.yml` does not.

What comes down from a dynamic include statement is its `vars:` and nothing else. Those
variables enter at the role-parameter layer, above facts and under `--extra-vars`, and they
accumulate through nested includes. An included task lands in the block and the section the
statement was written in, so a failing include inside a block body is caught by that block's
`rescue`.

`include_vars` is neither static nor dynamic in this sense: it produces variables rather than
steps, so it is an ordinary controller-side module, and what it reads lands at facts precedence.

A statement 32 levels deep is refused. One counter covers role nesting, `import_role` inside a
role's tasks and `import_tasks` inside an imported file, because a cycle can run through any
mixture of the three.

Where this differs from Ansible:

- A file that includes itself is refused at that ceiling and reported as a task failure at exit
  2. The reference recurses until the Python stack is gone, at exit 250.
- An included file that does not parse fails the task, at exit 2 and with a recap, where the
  reference exits 4 with no recap. A failure the recap can account for is worth more than a bare
  exit code, and it is also a shape a `rescue` can take.
- A failing include in a loop prints the `failed: [host] (item=...)` line every other failing
  loop item prints. The reference prints only an `[ERROR]` on standard error.
- `include_role` refuses `allow_duplicates`, `rolespec_validate` and `apply` by name, and
  refuses `public` by value: `public: false` is what this release does and is accepted, anything
  else is not. Honouring `public: true` would mean changing exported variable layers while the
  play's hosts stand at different steps.
- `include_vars` refuses `dir` and its six siblings by name. A run that quietly loaded none of a
  directory's variables would apply a playbook against defaults nobody wrote.
- `include_vars: file=x.yml name=ns` is read as one raw argument rather than split into
  `key=value` pairs, the same limitation `import_tasks` carries.
- What `include_vars` produces sits one rank above where the reference puts it, which is visible
  only for a name a host variable also carries.
- A failing `include_vars` carries `"failed": true` in its result body.

## `run_once` and `delegate_to`

`run_once` runs a task on one host of the batch and gives every other host what it produced,
including anything it registered. The hosts of the batch meet in front of such a task before any
of them runs it, because the election has to be unanimous and the readers have to wait for the
one host that runs.

`delegate_to` runs a task somewhere other than the host it is running for. The task travels on
the delegate's own link, under the delegate's connection settings, and the result is still
attributed to the host the play is working on: the recap counts that host, and the result line
reads `[h1 -> h3]`. A task a `when` skipped never reached the delegate, so `skipping:` carries
no arrow. Because the delegating host's own connection is never opened for such a task, a task
delegated away from a host that cannot be reached still runs.

`delegate_facts: true` writes what the task produced on the delegate instead of on the hosts
that registered it. A controller-side module such as `set_fact` or `debug` runs on the
controller whatever `delegate_to` says; the delegate then decides two things only, the name the
line shows and, under `delegate_facts`, whose facts are written.

The delegate is looked up in the whole inventory rather than in the play's hosts, so a play over
two hosts can delegate to a third that stays out of the recap. A name the inventory does not
have is connected to as an ssh target of that name, and reported unreachable if that fails. An
unreachable delegate takes the delegating host out of the run.

`delegate_to` and `delegate_facts` can be written on a block and on a task, not on a play, which
is where the reference accepts them too.

The two combine: a `run_once` task with a `delegate_to` runs once, on the delegate.

Where this differs from Ansible:

- `delegate_to` is rendered once per task rather than once per loop item. One batch is one
  message to one agent over one link, and a per-item delegate would split a loop across links.
- `local_action` is refused by name rather than run as a delegation to localhost.
  `delegate_to: localhost` does the same job and runs.
- When the host elected to run a `run_once` task cannot be reached, the hosts waiting on it
  leave the play and the run ends there. The reference prints one more empty play banner. There
  is no re-election in either engine, and the exit code is the same 4.
- `run_once` together with `delegate_facts` sets the facts on the delegate, which is the reading
  that keeps `delegate_facts` meaning one thing. The reference's behaviour here was not
  measured.
- `become_user: "{{ item }}"` fails the task, where the reference escalates per item. Escalation
  is settled once per batch here, for the same reason the delegate is.

## `environment` and `no_log`

`environment` sets variables for the process a module runs in. The play's layer goes down first,
then each enclosing block's from the outside in, then the task's, each rendered against that
item's variables and merged over the ones before it key by key. Values become what Python's
`str()` makes of them, so `42` is `"42"`, `true` is `"True"` and `null` is `"None"`. An
undefined variable inside an `environment` fails the task.

`no_log: true` replaces the task's output with the censored line `ansible-playbook` prints, on
every line it would have shown and at every verbosity: the `ok:` and `changed:` lines, the
`fatal:` line, an `UNREACHABLE!` line, each loop item's line, and a `debug`. A loop item's label
becomes `(censored due to no_log)`. Neither the task banner nor the task's name is hidden. The
registered variable and the recap keep the real result, so a later task can still read it.

Where this differs from Ansible:

- A non-mapping `environment` value is skipped with a warning naming the value, where the
  reference prints the whole layer stack it sat in. Naming the offending value is more use than
  naming the stack.
- That warning is not censored by `no_log`. Neither is the reference's, so this is parity rather
  than a regression, but it is worth knowing before putting a secret in an `environment` that
  might not be a mapping.

## What the pre-flight refuses

Volant's loader accepts the whole `ansible-core` 2.19 grammar, so a playbook parses here as it
parses there. Everything the loader accepted and this release cannot execute is then refused by
name, before the first connection and before any banner or recap. A keyword falling through both
gates would be ignored quietly, and the run would report success having skipped what you asked
for.

The refusal happens in two passes: once over the playbook as written, and once over the compiled
steps, which is the only place a keyword written inside a role or an imported file can be seen.
Every section of a block is walked, `rescue` and `always` included.

A refusal exits 4, which is what the reference exits for a playbook it cannot load. Refusals
whose own code was measured keep it: a role nobody can find and an unknown `meta` action or
`notify` name exit 1, and an escalation this release cannot perform exits 2.

[Keywords](keywords.md) is the list, generated from the tables the loader and the pre-flight
read, so it cannot describe a release other than this one. `check_mode: false` is accepted and
changes nothing, since running the task is what this release does; `check_mode: true` is refused,
because running a task for real when the playbook asked to be told what it would do is worse than
stopping.

Listings and `--syntax-check` never call the pre-flight, so a playbook can be read ahead of the
release that runs it.

Where this differs from Ansible:

- The first pass reads the play as written, so a task that `--tags` would have dropped is
  refused all the same; the second pass reads the compilation, where the selection has already
  happened. The asymmetry errs towards more refusals and never towards fewer.
- `tags` and `timeout` are refused before their value is looked at, so a `tags: 3` does not get
  the reference's type error.
- A `with_` key naming a real lookup plugin is refused by that name. The reference runs all
  twenty-five; only `with_items` has a loop here.
- A module this release cannot run has its arguments left unparsed, so an unknown module
  complains about the module and not about a `key=value` on the line below it.
- An invalid `meta` action is refused before the first connection rather than after the play
  banner. The sentence and the exit code are the reference's own 1, not the pre-flight's usual 4.
- `gather_facts`, `ignore_errors` and `become` written as something that is neither true nor
  false are refused when the playbook is read, one step earlier than the reference refuses them.
  The sentence and the exit code are the reference's.
- `refresh_inventory`, `clear_facts`, `clear_host_errors`, `reset_connection`, `end_host` and
  `end_play` are refused by name. Accepting a `meta` action that does nothing is the family of
  bug this split exists to close.
