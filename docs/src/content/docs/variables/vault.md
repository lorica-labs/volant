---
title: Vault
description: How Volant reads Ansible Vault data, where it gets the passwords, and why they never leave the controller process.
---

Volant reads and writes the vault 1.1 and 1.2 formats of ansible-core 2.19.12. The results, error messages and exit codes match the reference.

## Where the passwords come from

| Source | Notes |
|---|---|
| `-J`, `--ask-vault-pass` | Asked on the terminal, echo off. |
| `--vault-password-file`, `--vault-pass-file` | A file, or an executable that prints the password. Repeatable. |
| `--vault-id LABEL@SOURCE` | The source is a file, an executable or `prompt`. Repeatable. |
| `[defaults] vault_password_file`, `vault_identity_list` | The same, from `ansible.cfg`. |
| `ANSIBLE_VAULT_PASSWORD_FILE`, `ANSIBLE_VAULT_IDENTITY_LIST` | The same, from the environment. |

Every password is tried on every vaulted value, whatever its label.

## What is decrypted

- Inline `!vault` values in a playbook or a vars file. They load as ciphertext and decrypt when a template reads them. A decrypted value keeps the trust the reference gives it.
- Whole vaulted files: `vars_files`, `include_vars`, `group_vars` and `host_vars` files. A vaulted file that cannot be read fails with the reference's message.
- The `vault` and `unvault` filters, the `vault_encrypted` and `vaulted_file` tests, and the `unvault` lookup.

## Secrets stay on the controller

A vault password lives in memory in the controller process. It is never written to a command line, to the environment of a child process, to a log, to a result or to the agent link. The connection and escalation passwords follow the same rule. `-k` hands the SSH password to `ssh` through `SSH_ASKPASS` from a private temporary file that is removed at the end of the run. That needs OpenSSH 8.4 or later.

Filters, tests and lookups that run in the Python helper never see the vault password. The vault ones run in the controller process for that reason. See [Filters, tests and lookups in Python](/variables/python-helper/).

A value that came back from a managed host is never decrypted with the run's secrets, and never templated again. See [Trusted and untrusted values](/variables/trust/).

## Known gaps

- A vaulted `src` for `script` is not decrypted.
- A vaulted `src` for `unarchive` is not decrypted.
- `ansible_password` is not read.

The [command line page](/reference/cli/#vault) has the options.
