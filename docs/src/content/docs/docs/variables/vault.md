---
title: Vault
description: How Volant reads Ansible Vault data, where it gets the passwords, and why they never leave the controller process.
---

Volant reads and writes the vault 1.1 and 1.2 formats of ansible-core 2.19.12. The results, error messages and exit codes match ansible-core.

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

- Inline `!vault` values in a playbook or a vars file. They load as ciphertext and decrypt when a template reads them. A decrypted value keeps the trust ansible-core gives it.
- Whole vaulted files: `vars_files`, `include_vars`, `group_vars` and `host_vars` files. A vaulted file that cannot be read fails with ansible-core's message.
- The `vault` and `unvault` filters, the `vault_encrypted` and `vaulted_file` tests, and the `unvault` lookup.
- A vaulted `src` of `copy`, `template` and `script`, and a vaulted archive of `unarchive`. The run decrypts it into a private temporary directory before the plugin reads it. `decrypt: false` on `copy` or `unarchive` keeps the ciphertext.

A `!vault` value that no secret opens fails the task that reads it, with the three blocks ansible-core prints. `debug: var=x` exits 2, with the cause at the line where the value is written. `is vault_encrypted` answers for the variables of the current host.

## Secrets stay on the controller

A vault password lives in memory in the controller process. It is never written to a command line, to the environment of a child process, to a log, to a result or to the agent link. The connection and escalation passwords follow the same rule, and [Connections](/docs/hosts/connections/#connection-types) says how the SSH password reaches `ssh`.

Filters, tests and lookups that run in Python never see the vault password. The vault ones run in the controller process for that reason. See [Filters, tests and lookups that need Python](/docs/variables/templating/#filters-tests-and-lookups-that-need-python).

A value that came back from a managed host is never decrypted with the run's secrets, and never templated again. See [Values from hosts are never templated](/docs/variables/templating/#values-from-hosts-are-never-templated).

## Encrypting and viewing files

`volant vault encrypt`, `decrypt` and `view` work on the same files as `ansible-vault`: a file encrypted by one is read by the other, with the default id and with a label.

- `encrypt` replaces each file with its vault text and `decrypt` with its plaintext. Both leave the result at mode `0600` and print nothing; on a terminal they say `Encryption successful` or `Decryption successful` on standard error. A file without a final newline comes back without one.
- The header is `$ANSIBLE_VAULT;1.1;AES256` for the default id and `$ANSIBLE_VAULT;1.2;AES256;LABEL` for another. `--vault-id LABEL@SOURCE` sets the label. With more than one `--vault-id`, `--encrypt-vault-id` (or `vault_encrypt_identity`, `ANSIBLE_VAULT_ENCRYPT_IDENTITY`) picks the one that encrypts; without it the command exits 5.
- `--output FILE` writes there and leaves the source alone, `--output -` writes to standard output, and no file name, or `-`, reads standard input.
- `view` prints each file's plaintext. On a terminal it goes through the pager: `PAGER`, else `ANSIBLE_PAGER`, else `[defaults] pager`, else `less`.
- Without any password source the command asks for it. `encrypt` asks twice, as `New Vault password: ` and `Confirm New Vault password: `.
- `-v` prints the configuration file in use and `-vv` the version. The extra lines of `-vvvv` are not printed.

## Creating, editing and rekeying files

`volant vault create`, `edit` and `rekey` work on the same files as `ansible-vault`.

- `create` takes one file name. It refuses with exit code 5 unless standard output is a terminal or `--skip-tty-check` is given, and with exit code 1 when the file exists. A missing directory is created, with a warning. The editor can leave the file empty: the result is a valid vault file holding an empty text.
- `edit` decrypts each file, opens the editor and encrypts the result with the secret that opened the file and the label it had. When the text did not change, the file is not rewritten. A changed file keeps its mode.
- The editor is `EDITOR`, else `ANSIBLE_EDITOR`, else `[defaults] editor`, else `vi`, split into words as a shell would split it, with the file name added. Its exit status is not looked at, as in ansible-vault.
- The editor works on a copy with mode `0600` in a private directory under `ANSIBLE_LOCAL_TEMP` (`[defaults] local_tmp`, else `~/.ansible/tmp`). The copy is overwritten and removed on every way out, including a failing editor and SIGTERM or SIGHUP. Ctrl-C goes to the editor.
- `rekey` decrypts each file with the old secrets and writes it again with the new one, keeping its mode. The new secret comes from `--new-vault-password-file`, from `--new-vault-id LABEL@SOURCE`, or from the prompts `New Vault password: ` and `Confirm New Vault password: `. It prints `Rekey successful` on standard error, whatever standard output is.

## Known gaps

- The `salt` and `wrap_object` arguments of the `vault` filter are refused as not supported yet.
- A `!vault` value written in a play's own `vars:` prints no `Origin:` line when no secret opens it.
- A `set_fact` of a bare `{{ x }}` that holds a vaulted value loses the vault tag, so `is vault_encrypted` answers false on the fact where ansible-core answers true.

The [command line page](/docs/reference/cli/#vault) has the options.
