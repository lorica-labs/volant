---
title: Handlers
description: When handlers run, how notify finds them, and what force_handlers changes.
---

A handler runs at a flush point, once, however many tasks notified it.

```yaml
tasks:
  - name: Update the nginx configuration
    lineinfile:
      path: /etc/nginx/nginx.conf
      regexp: '^worker_processes'
      line: 'worker_processes auto;'
    notify: restart nginx

handlers:
  - name: restart nginx
    systemd_service:
      name: nginx
      state: restarted
```

## Flush points

Every play has three automatic flush points: after `pre_tasks`, after the roles and `tasks`, and after `post_tasks`. `meta: flush_handlers` adds one wherever you write it, with the `TASK [meta]` banner Ansible prints there.

A flush point inside an `include_tasks` file runs only for the hosts whose own statement included that file. The other hosts report the step, run no handlers there, and keep their notifications for the next flush they reach.

## How notify finds a handler

`notify` names a handler. The name reaches the first handler whose `name` matches, and every handler that lists it under `listen`. This asymmetry comes from Ansible, and it is why a role applied three times contributes one handler rather than three.

Handlers run in the order they are defined, not the order they were notified. A role named in `roles:` or by `import_role` puts its handlers in front of the play's own. A role brought in by `include_role` puts them behind, as in Ansible.

A task notifies only when it reports `changed`. For a loop, the overall result decides, so one changed item is enough. Once a flush has run, the notifications are cleared, so a handler does not run again at the next flush unless something notifies it again.

## When a handler fails

A failing handler removes the host like any other failing task, and it stops the handlers after it in the same flush. A flush written inside a block belongs to that block, so a handler that fails there is caught by the block's `rescue`.

## force_handlers

By default, a host that failed does not run the handlers it was notified for. Any of these makes it run them:

- `force_handlers: true` on the play
- `force_handlers = true` in the `[defaults]` section of `ansible.cfg`
- `--force-handlers` on the command line

The play keyword takes priority over the other two. Under `force_handlers`, a failed host walks the rest of the play, reports every step and runs the handlers it was notified for. The recap still counts the failure.

Handlers do not appear in any listing, and neither do the three automatic flush points.

## Differences from Ansible

- A `notify` that names a handler that does not exist is rejected before the first connection, with the reference's own message and exit code 1. Ansible prints the notifying task's banner first. With Volant, nothing has run when you read the error.
- The one exception is a `notify` in a file a dynamic include brings in, which only a running host reaches. It is checked when the task reports `changed` without failing, as in Ansible, and it ends the run with exit code 1: `The requested handler '<name>' was not found in either the main handlers list nor in the listening handlers list`. No `ignore_errors` and no `rescue` can catch it.
- A `notify` whose value is a template is rejected with exit code 4. Resolving it per host and per loop item would mean failing a task that has already printed its result. In a file a dynamic include brings in, it fails that include instead, with `a templated 'notify' is not supported yet`.
- A block inside a `handlers:` list is rejected. A flush has nowhere to put a block's `rescue`.
- A `force_handlers` value that is neither true nor false leaves the setting unchanged. Ansible reads it as false.
