# Connections, the agent cache and privilege escalation

Volant reaches a managed host, puts a small static agent there, and talks to it over the same
connection for the rest of the run. If you are trying to work out whether your playbook can run
under Volant today, the last section on this page lists what is still missing.

## Connection types

`ansible_connection` selects the transport. It defaults to `ssh`, as it does in Ansible.

- `ssh` runs the OpenSSH client from your `PATH`. Volant builds the command line itself; it does
  not shell out through a wrapper, and it does not link a Rust SSH library. Your `~/.ssh/config`
  applies, because the `ssh` binary reads it.
- `local` runs the agent as a child process on the machine running Volant, without any network.

Any other value is refused by name, and the host is reported unreachable while the rest of the
run continues.

Only key files and keys held by an ssh-agent work. `ssh` runs with `BatchMode=yes`, so it never
prompts: a host that would ask for a password or a passphrase is reported unreachable instead of
stopping the run at a prompt. `-k` and `ansible_password` are not implemented, because OpenSSH
cannot take a password on its standard input without `sshpass`.

## Host variables

These are read per host, from the inventory or from any other variable source:

| Variable | Effect |
| --- | --- |
| `ansible_connection` | `ssh` (default) or `local` |
| `ansible_host` | Address to connect to; defaults to the inventory name |
| `ansible_port` | `ssh -p`; a quoted number works as well as a bare one |
| `ansible_user` | `ssh -l` |
| `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_ssh_common_args` | Extra `ssh` arguments, split like a shell would |
| `ansible_ssh_extra_args` | The same, applied after the common ones |
| `ansible_remote_tmp` | Directory holding the agent cache on that host |
| `ansible_become` | Escalate or not, overriding both keywords |
| `ansible_become_user` | Target user for escalation |
| `ansible_become_method` | Must be `sudo` |
| `ansible_become_password`, `ansible_become_pass` | Escalation password |

An unbalanced quote in `ansible_ssh_common_args` or `ansible_ssh_extra_args` fails the host
rather than being dropped. That matters on a bastion topology: silently discarding a
`ProxyCommand` would connect straight to an address that may hold a different machine.

## Settings from `ansible.cfg`

Volant looks for `ansible.cfg` where `ansible-playbook` looks for it: `ANSIBLE_CONFIG`, then
`./ansible.cfg`, `~/.ansible.cfg`, `/etc/ansible/ansible.cfg`. The matching environment variable
wins over the file, and a command-line flag wins over both.

| Key | Section | Environment | Flag | Default |
| --- | --- | --- | --- | --- |
| `inventory` | `defaults` | `ANSIBLE_INVENTORY` | `-i` | none |
| `timeout` | `defaults` | `ANSIBLE_TIMEOUT` | `-T` | 10 seconds |
| `remote_user` | `defaults` | `ANSIBLE_REMOTE_USER` | `-u` | whatever `ssh` picks |
| `private_key_file` | `defaults` | `ANSIBLE_PRIVATE_KEY_FILE` | `--private-key` | none |
| `host_key_checking` | `defaults` | `ANSIBLE_HOST_KEY_CHECKING` | none | true |
| `remote_tmp` | `defaults` | `ANSIBLE_REMOTE_TMP` | none | `~/.ansible/tmp` |
| `forks` | `defaults` | `ANSIBLE_FORKS` | `-f` | 5 |
| `become` | `privilege_escalation` | `ANSIBLE_BECOME` | `-b` | false |
| `become_user` | `privilege_escalation` | `ANSIBLE_BECOME_USER` | `--become-user` | `root` |
| `become_method` | `privilege_escalation` | `ANSIBLE_BECOME_METHOD` | `--become-method` | `sudo` |

`timeout` becomes `ConnectTimeout` on the `ssh` command line. With `host_key_checking` off,
`StrictHostKeyChecking=no` and `UserKnownHostsFile=/dev/null` are added, so an unknown key is
accepted and not written anywhere; with it on, an unknown key refuses the host.

`forks` bounds how many hosts a play works on at once. A `forks` of zero or a value that is not
a number is refused before the run starts, and the current value is readable in a playbook as
`ansible_forks`.

## The agent cache

The agent is a single static executable. Volant uploads it once per host and reuses it on every
later run.

It lives at `<remote_tmp>/volant-agent-<version>/volant-agent`, so with the default `remote_tmp`
that is under `~/.ansible/tmp/` in a home directory. The `<version>` in that path is the
controller's own version, which is how an upgraded controller avoids running an older agent.

Whose home directory depends on who runs the agent. A task that does not escalate runs it as the
account you log in as, and caches it there. A task that escalates runs it as the `become_user`,
and caches it in that user's own home, written through `sudo` on the way in. That is what lets a
`become_user` other than `root` work at all: a home directory at mode 0700, which is the default
on several distributions, is one no other account can read, so an agent cached under the account
you log in as would be out of reach of everybody you might become. One host therefore holds one
cache per account the run acts as, each owned by that account and readable by nobody else.

The agent is a few hundred kilobytes. It sits on the host uncompressed, since an executable has
to be uncompressed to run, and it travels through `ssh -C`, so OpenSSH compresses it on the wire
and nothing has to be unpacked before a run.

Before every connection Volant asks the cached agent for its version. A version that does not
match, a missing file and a file that will not run at all each lead to a fresh upload, which means
a cached agent that refuses to start gets repaired instead of reported. That also covers a copy an
interrupted run left truncated.

The upload checks free space with `df` first, wanting room for twice the agent plus a megabyte,
and says what it needed and what it found when there is not enough. It writes to a temporary name
carrying the remote shell's process id, verifies the byte count, then renames into place, so a
transfer cut short never lands under the final name and two runs uploading to one host at the same
time cannot interleave into a single file.

To clear the cache, remove the directory: `ssh <host> 'rm -rf ~/.ansible/tmp/volant-agent-*'`.
Nothing else is left behind. An upload already removes the cache directories of other versions, so
a host holds one agent rather than a history of them.

## Persistent connections

A host keeps its agent link for the whole run, across plays. Tasks travel as batches over that
one link instead of one connection per task. If a link dies between two plays, Volant reconnects
once; a second failure reports the host unreachable.

Escalation adds a link rather than replacing one: while a batch runs, a host that escalates
holds two links, one for the user that batch escalates to and one for the account you log in as.
Only that second one lives longer than the batch. What persistence is for is the connection to
the host itself, and that is the link that stays.

An escalated link closes as soon as its batch is answered, and is reopened by the next batch
that needs it. That is more often than once per play: a task carrying `register`, `loop`,
`changed_when` or `failed_when` ends the batch it is in, and so does a task reading another
host's variables or escalating to a different user. A play built from those pays for a fresh
escalated link at each one. The reopen is one probe and one `ssh`, because the agent is already
cached for that user, so it still beats the connection per task the same play costs under
Ansible, but it is not free: grouping escalated work into runs of plain tasks is what keeps the
count down.

Each link is a separate `ssh` process and costs the controller three file descriptors, so a run
is limited by `ulimit -n` as well as by the hosts it names. `forks` bounds the escalated links,
since a host holds one only while it is one of the hosts working; the link to the host itself is
held from the play that first reached it to the recap, so a run of N hosts escalating to any
number of users settles at N plus `forks` links. That is the figure it settles at, not a ceiling
it never crosses: a link being closed gets up to two seconds to tell its agent to stop, and it is
closed off to one side rather than waited for, so a play working through batches quickly can hold
a few descriptors more than the arithmetic says for as long as those closes take. With the usual
`ulimit -n 1024` the room runs out past roughly 338 links, and `ssh` then stops starting: the
hosts it happens to hit are reported unreachable although nothing is wrong with them. Raise
`ulimit -n` before a run that wide.

An escalated link also costs one extra `ssh` connection for the `sudo` probe, two when a
password is wanted. There is no `ControlMaster` yet, so a wide inventory pays for every one of
those connections separately.

## Privilege escalation

`become` runs tasks as another user through `sudo`. The agent for that user is started as
`sudo -H <form> -u <user> -- volant-agent`, where `<form>` is `-n` or `-k -S -p ''` as the probe
below decides, and tasks that do not escalate keep using the unprivileged agent. See
[ADR 0004](https://github.com/lorica-labs/volant/blob/main/docs/adr/0004-become-runs-a-second-agent-under-sudo.md)
for why it works this way.

The `become`, `become_user` and `become_method` keywords are accepted on a play and on a task.
Precedence, measured against the reference: a host's `ansible_become` and `ansible_become_user`
variables beat both keywords in either direction, the task keyword beats the play keyword, and
the configuration defaults speak last.

`become_method` must be `sudo`. Anything else is refused by name before a task runs, rather than
quietly escalating through a mechanism you did not ask for. `ansible_become_flags` and
`become_exe` are not read.

A password comes from `-K` (`--ask-become-pass`), which turns the terminal echo off on unix and
reads the line as it comes elsewhere, or from
`ansible_become_password` / `ansible_become_pass`. It is written once to `sudo`'s standard input
and appears in no command line, no log and no result: the types that carry it print `<redacted>`
in a diagnostic, and `sudo`'s own error output is scrubbed before it reaches a message.

Volant asks `sudo` which form to use rather than assuming one, because a `sudo` that needs no
authentication does not read its standard input at all. Escalation that fails, for a missing
password, a wrong one, or a `sudoers` rule that forbids the command, fails the task and leaves
the host reachable, with the reference's own wording: `Missing sudo password` or
`Incorrect sudo password`.

## Host patterns

A pattern, in a play's `hosts` or in `--limit`, follows Ansible's grammar:

- Terms separated by `,` or `:`.
- `all` and `*` for every host.
- Shell wildcards on host and group names: `web*`, `db?`, `host[abc]`, `host[a-z]`.
- `!term` removes hosts, `&term` keeps only hosts that also match.
- `name[N]`, `name[N:M]` and `name[N-M]` index or slice the hosts a term produced. As in
  Ansible, `M` is included, and `name[N:]` runs from `N` to the last host.

Terms are not applied in the order you write them: plain terms first, then `&`, then `!`, which
is what Ansible does. A pattern made only of `!` or `&` terms starts from `all`.

A name that is both a host and a group means the host. Volant warns once per run when an
inventory contains such a name, whatever pattern the run uses, because the reference detects it
at load time.

`localhost` exists implicitly with a local connection when the inventory does not define it.

## Not there yet

- Roles and collections beyond the native modules.
- Python modules: only the [native modules](modules.md) run.
- Facts. `gather_facts` is accepted and warns; no `ansible_*` fact is ever defined.
- `serial`, `run_once`, `delegate_to`, `until`, `block`/`rescue`, `include_*` and `import_*`.
- `become_method` other than `sudo`; `ansible_become_flags` and `become_exe`.
- SSH passwords (`-k`, `ansible_password`).
- Windows and network devices.
