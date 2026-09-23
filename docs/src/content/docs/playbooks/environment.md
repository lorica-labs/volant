---
title: Environment and no_log
description: Setting environment variables for a module, and hiding a task's output.
---

## environment

`environment` sets variables for the process a module runs in.

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

Layers are merged key by key, each one over the previous: the play first, then each enclosing block from the outside in, then the task. Each layer is rendered with the variables of the current loop item.

Values become what Python's `str()` makes of them, so `42` becomes `"42"`, `true` becomes `"True"` and `null` becomes `"None"`. An undefined variable inside `environment` fails the task.

## no_log

`no_log: true` replaces the task's output with the censored line `ansible-playbook` prints, on every line it would have shown and at every verbosity level. That covers the `ok:` and `changed:` lines, `fatal:`, `UNREACHABLE!`, each loop item's line, and `debug` output. A loop item's label becomes `(censored due to no_log)`.

The task banner and the task's name are not hidden. The registered variable and the recap keep the real result, so a later task can still read it.

## Differences from Ansible

- An `environment` value that is not a mapping is skipped with a warning that names the value. Ansible prints the whole stack of layers it sat in instead.
- That warning is not censored by `no_log`. Ansible does not censor its warning either, so this matches the reference, but keep it in mind before you put a secret in an `environment` that might not be a mapping.
