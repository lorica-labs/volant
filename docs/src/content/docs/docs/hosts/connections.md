---
title: Connections and become
description: How Volant reaches a host, the host variables and ansible.cfg settings it reads, and how become works through sudo and su.
---

This page answers how Volant reaches your hosts, which variables control the connection, and how tasks run as another user with `become`.

Volant reaches a managed host, puts a small static agent there (a program that runs your tasks and reports back), and talks to it over the same connection for the rest of the run.

## Connection types

`ansible_connection` picks the transport. It defaults to `ssh`, as in Ansible.

| Value | What it does |
|---|---|
| `ssh` | Runs the OpenSSH client from your `PATH`. Volant builds the command line itself, without a wrapper and without a Rust SSH library, so your `~/.ssh/config` applies. |
| `local` | Runs the agent as a child process on the controller, with no network. |
| `docker` | Runs the agent in a running container through `docker exec -i`, as `community.docker.docker` does. See [Docker containers](#docker-containers). |
| `podman` | Runs the agent in a running Podman container through `podman exec -i`, rootless or not, as `containers.podman.podman` does. See [Podman containers](#podman-containers). |
| `chroot` | Runs the agent in a directory tree on the controller through `chroot`, as `community.general.chroot` does. See [Chroot directories](#chroot-directories). |
| `buildah` | Runs the agent in a Buildah working container through `buildah run`, as `containers.podman.buildah` does. See [Buildah working containers](#buildah-working-containers). |
| `lxd` | Runs the agent in an LXD instance through `lxc exec`, as `community.general.lxd` does; `community.general.incus` does the same for Incus. See [LXD and Incus instances](#lxd-and-incus-instances). |
| `kubectl` | Runs the agent in a container of a Kubernetes pod through `kubectl exec`, as `kubernetes.core.kubectl` does. See [Kubernetes pods](#kubernetes-pods). |
| `aws_ssm` | Runs the agent on an EC2 instance through an AWS Systems Manager session, as `amazon.aws.aws_ssm` does. See [AWS Systems Manager sessions](#aws-systems-manager-sessions). |
| `paramiko_ssh` | Runs the OpenSSH client in place of ansible-core's deprecated paramiko connection; `paramiko` is the same. See [Paramiko](#paramiko). |

Any other name is resolved by the controller's ansible-core, the way `ansible-playbook` resolves it, once per name and per run. `ansible.builtin.ssh` and `ansible.builtin.local` resolve to the two plugins above and run. A short name that a collection took over from ansible-core is routed to that collection when it is installed:

| Short name | Resolves to | Collection in the `ansible` 12.3.0 package |
|---|---|---|
| `docker` | `community.docker.docker` | community.docker 4.8.5 |
| `podman` | `containers.podman.podman` | containers.podman 1.18.0 |
| `buildah` | `containers.podman.buildah` | containers.podman 1.18.0 |
| `kubectl` | `kubernetes.core.kubectl` | kubernetes.core 6.2.0 |
| `lxd` | `community.general.lxd` | community.general 11.4.2 |
| `chroot` | `community.general.chroot` | community.general 11.4.2 |
| `aws_ssm` | `community.aws.aws_ssm`, which `community.aws` 10.0.0 sends on to `amazon.aws.aws_ssm` | amazon.aws 10.1.2 |
| `incus` | no short name: write `community.general.incus` | community.general 11.4.2 |

With `-vvv`, a routed name prints `redirecting (type: connection) ansible.builtin.chroot to community.general.chroot` under the first task that uses it on a host, one line per hop.

A name that resolves to a plugin Volant does not drive itself, such as `winrm`, `psrp`, `community.general.jail` or a plugin in a `connection_plugins/` directory beside the playbook, takes the second route: see [Connections only ansible-core has](#connections-only-ansible-core-has). Without the controller's ansible-core to ask, such a name makes the host unreachable with `connection '<full name>' is not supported`, and the rest of the run continues. The [support matrix](/docs/reference/parity/#collection-connection) lists every connection plugin of the collections in the package.

A name that resolves to nothing fails the task with `the connection plugin '<name>' was not found`, as Ansible does. That includes a short name routed to a collection that is not installed, such as `docker` without community.docker, after the warning ansible-core prints for it, and `ansible.builtin.paramiko`, which ansible-core 2.19.12 does not carry under that name. That applies whether the value is the host's own connection, a `delegate_to` target's, or that of the host running a `run_once` task.

A password comes from `-k`, from `--connection-password-file` or from the `ansible_password` variable. Volant hands it to `ssh` through `SSH_ASKPASS` and a private temporary file that is removed at the end of the run, which needs OpenSSH 8.4 or later. `ssh` then runs with `BatchMode=no` and `NumberOfPasswordPrompts=1`, so one wrong answer ends the attempt. The password is never on a command line, in the environment of a program or in the output, `-vvvv` included. A vaulted `ansible_password` works like any vaulted variable. Without a password, `ssh` runs with `BatchMode=yes` and never prompts, so a host that would ask for a password or a passphrase is reported unreachable instead of hanging the run.

## Docker containers

`ansible_connection: docker`, or `community.docker.docker`, runs a play inside a running container, with no sshd in it and no network path to it. Both names need community.docker installed on the controller, as they do for `ansible-playbook`. Volant runs the `docker` program from your `PATH`, so `DOCKER_HOST`, `DOCKER_CONTEXT` and `DOCKER_CONFIG` reach it through the environment.

The agent goes up through `docker exec -i`, so the image needs `/bin/sh` and the usual tools: `uname`, `id`, `ls`, `mkdir`, `df`, `awk`, `head`, `wc`, `chmod`, `mv` and `find`. A minimal Alpine or Debian image has them. An image with no shell, such as distroless, gets the agent through `docker cp` instead: Volant asks `docker inspect` for the image's architecture, copies the agent into the container's `/tmp`, and the agent puts itself in its cache directory under the same checks. The container needs a `/tmp`, `remote_tmp` must be absolute or start with `~` for a user that has a home, and `become` is refused, since there is no shell to run `sudo` from. The copy in `/tmp` stays when the container's user cannot remove it.

| Variable | What it does |
|---|---|
| `ansible_docker_host` | The container's name or id. It beats `ansible_host`, which beats the inventory name. |
| `ansible_docker_user` | The user commands run as, `docker exec -u`. It beats `ansible_user`, the `remote_user` keyword and `-u`. Unset, the container's own user. |
| `ansible_docker_extra_args` | Arguments put after `docker` and before `exec`, split on single spaces rather than as shell words. The default comes from `[docker_connection] extra_cli_args`. |
| `ansible_docker_extra_env` | A mapping of variables set with `docker exec -e`. Every value must be text: an unquoted number or boolean is refused. |
| `ansible_docker_privileged` | `docker exec --privileged`. The default comes from `ANSIBLE_DOCKER_PRIVILEGED` or `[docker_connection] privileged`. |
| `ansible_docker_working_dir` | `docker exec -w`. The default comes from `ANSIBLE_DOCKER_WORKING_DIR` or `[docker_connection] working_dir`. |
| `ansible_docker_timeout` | Seconds `docker` may take to answer the first command. The default comes from `ANSIBLE_DOCKER_TIMEOUT`, then `[docker_connection] timeout`, then the connection timeout. |

`become` with `sudo` works through the container's own `sudo`. `su` is refused, because `docker exec` gives it no terminal. A container that does not exist or is stopped, or a user the image does not have, makes the host unreachable with docker's own message. ansible-core reports all three as `Failed to create temporary directory`, so this message differs on purpose.

With `-vvvv` the lines show `ESTABLISH DOCKER CONNECTION FOR USER: <user>`, then one `EXEC [...]` line per command, which are Volant's own commands. When the play names no user, the user comes from `docker inspect`: `root` when the image names none, `?` when docker cannot say.

## Podman containers

`ansible_connection: podman`, or `containers.podman.podman`, runs a play in a Podman container, rootless or not, with no daemon and no ssh. Both names need containers.podman installed on the controller. The agent goes up as it does for docker: through `podman exec -i` when the image has a shell, through `podman cp` when it has none.

Volant reads the options the way the plugin's source reads them, which differs from its documentation. The container is `ansible_host`, else the inventory name; the documented `ansible_podman_host` is not read. The user is `ansible_user`, the `remote_user` keyword or `-u`, passed as `podman exec --user`.

| Variable | What it does |
|---|---|
| `ansible_podman_executable` | The `podman` program, a name looked up on your `PATH` or a path. The default comes from `ANSIBLE_PODMAN_EXECUTABLE`. |
| `ansible_podman_extra_args` | Arguments put after `podman` and before the subcommand, split the way a shell would split them. The default comes from `ANSIBLE_PODMAN_EXTRA_ARGS` or `[defaults] podman_extra_args`. |

The plugin runs `podman exec` with no `-i`, so it cannot hand a command its stdin. The agent needs one, so Volant runs `podman exec -i`; that is the one word its command lines add. Volant never runs `podman mount`. `become` with `sudo` works through the container's own `sudo`, and `su` is refused. With `-vvvv` each command shows as a `RUN [...]` line, as the plugin prints it, with no `ESTABLISH` line.

## Chroot directories

`ansible_connection: chroot`, or `community.general.chroot`, runs a play inside a directory tree on the controller: an image build, a `debootstrap` tree, a rescue. The inventory name, or `ansible_host`, is the directory. Both names need community.general installed on the controller. The run needs root, because `chroot` does.

| Variable | What it does |
|---|---|
| `ansible_chroot_exe` | The `chroot` program, a name looked up on your `PATH` or an absolute path. The default comes from `ANSIBLE_CHROOT_EXE` or `[chroot_connection] exe`. |
| `ansible_chroot_disable_root_check` | Skip the check that the controller runs as root. The default comes from `ANSIBLE_CHROOT_DISABLE_ROOT_CHECK` or `[chroot_connection] disable_root_check`. |
| `ansible_executable` | The shell commands run under, `/bin/sh` by default. The default comes from `ANSIBLE_EXECUTABLE` or `[defaults] executable`. |

Before anything runs, the task fails, as it does under ansible-core, for a path that is not a directory, for a tree with no `bin/sh`, and for a controller that is not root unless the check is off. The agent goes up through `chroot <dir> /bin/sh`, so the script that writes it runs inside the tree, and no path it writes can leave the tree, whatever symbolic links the tree holds. When `df` cannot say how much room there is, as in a tree with no `/proc` mounted, the space check is left out. `become` works when the tree has `sudo`. With `-vvvv` the lines show `THIS IS A LOCAL CHROOT DIR`, then one `EXEC [...]` line per command.

## Buildah working containers

`ansible_connection: buildah`, or `containers.podman.buildah`, configures the working container `buildah from <image>` makes, so a play can build an image layer by layer, which `buildah commit` then saves. Both names need containers.podman installed on the controller. As for podman, the container is `ansible_host`, else the inventory name, and the user is `ansible_user`, the `remote_user` keyword or `-u`; `root` adds no flag. The program is the `buildah` on your `PATH`, and the `BUILDAH_*` variables reach it through the environment.

The agent starts with `buildah run [--user U] <container> -- <agent>`. Volant never runs `buildah mount`, which needs `buildah unshare` for a rootless user: a working container with no shell gets the agent through `buildah copy`. Rootless buildah prints warnings about capabilities on every command it runs, so the agent's stderr is not shown. With `-vvvv` each command shows as a `RUN [...]` line, with no `ESTABLISH` line.

## LXD and Incus instances

`ansible_connection: lxd`, or `community.general.lxd`, runs a play in an LXD instance, local or on a remote server, through the `lxc` command. `community.general.incus` does the same for Incus with the `incus` command; it has no short name in ansible-core. Both need community.general installed on the controller.

| Variable | What it does |
|---|---|
| `ansible_lxd_host`, `ansible_incus_host` | The instance, up to its first dot: `web.example.com` is `web`. It beats `ansible_host`, which beats the inventory name. |
| `ansible_lxd_remote`, `ansible_incus_remote` | The remote server, `local` by default. |
| `ansible_lxd_project`, `ansible_incus_project` | The project. Incus always passes one, `default` by default. |
| `ansible_lxd_executable`, `ansible_incus_executable` | The shell commands run under. It beats `ansible_executable`; `/bin/sh` by default. |

The user is `ansible_user`, the `remote_user` keyword or `-u`, `root` by default. For any other user, every command goes through the become method, `/bin/su <user> -c` unless `lxd_become_method` or `incus_become_method` names another. An instance with no shell gets the agent through `lxc file push` or `incus file push` when the user is root. A missing instance and a stopped one make the host unreachable with the plugin's words, such as `instance not running: web`; Incus adds the remote and the project. With `-vvvv` the lines show `ESTABLISH LXD CONNECTION FOR USER: <user>` (`Incus` for Incus), the plugin's `INFO:` line for a user other than root, then one `EXEC [...]` line per command.

## Kubernetes pods

`ansible_connection: kubectl`, or `kubernetes.core.kubectl`, runs a play in a container of a pod through `kubectl exec -i`. Both names need kubernetes.core installed on the controller and `kubectl` on your `PATH`.

| Variable | What it does |
|---|---|
| `ansible_kubectl_pod` | The pod. Without it, the pod is `ansible_host`, else the inventory name. |
| `ansible_kubectl_container` | The container, for a pod that has more than one. |
| `ansible_kubectl_namespace` | The namespace. |
| `ansible_kubectl_kubeconfig`, `ansible_kubectl_config` | The kubeconfig file. A kubeconfig given as a mapping goes to a private temporary file, removed at the end of the run. |
| `ansible_kubectl_extra_args` | More arguments for `kubectl`, split on spaces. |
| `ansible_kubectl_local_env_vars` | A mapping of variables set in the environment of every `kubectl` command. |
| `ansible_kubectl_context` | The context in the kubeconfig. |
| `ansible_kubectl_server`, `ansible_kubectl_host` | The API server. |
| `ansible_kubectl_username`, `ansible_kubectl_user`, `ansible_kubectl_password` | A user name and password for the API server. |
| `ansible_kubectl_token`, `ansible_kubectl_api_key` | A bearer token for the API server. |
| `ansible_kubectl_client_cert`, `ansible_kubectl_cert_file`, `ansible_kubectl_client_key`, `ansible_kubectl_key_file` | The client certificate and its key. |
| `ansible_kubectl_ca_cert`, `ansible_kubectl_ssl_ca_cert` | The certificate authority of the API server. |
| `ansible_kubectl_validate_certs`, `ansible_kubectl_verify_ssl` | Given as it is after `--insecure-skip-tls-verify`, as the plugin does: `true` gives `True`, and `false` adds nothing. |

Each option can also come from the `K8S_AUTH_*` environment variable the plugin reads, which a host variable beats. The flags reach `kubectl` in the plugin's order. Commands run as the container's user: the plugin cannot pick one, so `ansible_user` is ignored. A container with no shell cannot take the agent, since `kubectl cp` needs `tar` in it. A missing pod, a missing namespace and a finished pod make the host unreachable with what `kubectl` said. With `-vvvv` the lines show `ESTABLISH kubectl CONNECTION`, then one `EXEC [...]` line per command, with the password and the token shown as `********`.

## AWS Systems Manager sessions

`ansible_connection: amazon.aws.aws_ssm` runs a play on an EC2 instance through AWS Systems Manager Session Manager, with no inbound port, key pair or bastion. The short name `aws_ssm` reaches it through community.aws, so that collection is needed too. The controller needs `session-manager-plugin` and boto3 in its Python, and the instance needs the SSM agent.

| Variable | What it does |
|---|---|
| `ansible_aws_ssm_instance_id` | The instance. Without it, the instance is `ansible_host`, else the inventory name. |
| `ansible_aws_ssm_region` | The region, else `AWS_DEFAULT_REGION` or `AWS_REGION`; `us-east-1` by default. |
| `ansible_aws_ssm_profile` | The AWS profile, else `AWS_PROFILE`. |
| `ansible_aws_ssm_access_key_id`, `ansible_aws_ssm_secret_access_key`, `ansible_aws_ssm_session_token` | The credentials, else the `AWS_*` environment variables of the same names, else what boto3 finds. They are never printed, `-vvvv` included. |
| `ansible_aws_ssm_plugin` | The `session-manager-plugin` program, else `AWS_SESSION_MANAGER_PLUGIN`, `/usr/local/bin/session-manager-plugin` or the one on your `PATH`. |
| `ansible_aws_ssm_document` | The SSM document the session starts; the account's default shell when unset. |
| `ansible_aws_ssm_timeout` | How many seconds the session has to answer each step of the connection; 60 by default. |

Commands run as the session's user, `ssm-user` on Amazon Linux. `become` picks another user, and `ansible_user` is ignored, as with the plugin. Volant sends nothing through S3, so the `ansible_aws_ssm_bucket_*` variables the plugin needs are not read. The session's terminal only hands back valid UTF-8, so the agent talks to the controller in base64 lines, at about 400 KiB/s each way, and the first run on an instance uploads the agent once, in about ten seconds. An instance the SSM agent does not manage, or credentials that are refused, make the host unreachable with boto3's words, where the plugin fails the task. When the run ends, or the controller is killed, the session closes and the agent stops with it. With `-vvvv` the lines show `ESTABLISH SSM CONNECTION TO: <instance>`, then one `EXEC: <command>` line per command.

## Paramiko

`ansible_connection: paramiko_ssh` is ansible-core's connection through the Python paramiko library. ansible-core 2.19.12 marks it deprecated and removes it in 2.21. Volant runs these hosts with the `ssh` program instead, so paramiko does not need to be installed. Each option of the plugin becomes the closest `ssh` option, and the first host to connect prints ansible-core's warning once per run:

```text
[DEPRECATION WARNING]: The paramiko connection plugin is deprecated. This feature will be removed from ansible-core version 2.21.
```

`deprecation_warnings = False` silences it. `paramiko`, `_paramiko_ssh` and `ansible.builtin.paramiko_ssh` are the same plugin.

| Variable | What it does |
|---|---|
| `ansible_paramiko_host` | The address. It beats `ansible_ssh_host`, which beats `ansible_host`, then the inventory name. |
| `ansible_paramiko_port` | `ssh -p`. It beats `ansible_ssh_port` and `ansible_port`, then the `port` keyword, `ANSIBLE_REMOTE_PARAMIKO_PORT`, `ANSIBLE_REMOTE_PORT`, `[paramiko_connection] remote_port` and `[defaults] remote_port`. |
| `ansible_paramiko_user` | `ssh -l`. It beats `ansible_ssh_user` and `ansible_user`, then the `remote_user` keyword, `ANSIBLE_PARAMIKO_REMOTE_USER`, `[paramiko_connection] remote_user` and the usual remote user settings. |
| `ansible_paramiko_private_key_file` | `ssh -i`. It beats `ansible_ssh_private_key_file` and `ansible_private_key_file`, then `ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE`, `[paramiko_connection] private_key_file` and the usual key settings. |
| `ansible_paramiko_password`, `ansible_paramiko_pass` | The password, handed to `ssh` as for an `ssh` host. They beat `ansible_ssh_password`, `ansible_ssh_pass` and `ansible_password`. |
| `ansible_paramiko_timeout` | `ConnectTimeout`. It beats `ansible_ssh_timeout`, then `ANSIBLE_PARAMIKO_TIMEOUT`, `ANSIBLE_SSH_TIMEOUT`, `[paramiko_connection] timeout`, `[ssh_connection] timeout` and the connection timeout. `ansible_timeout` is not read, as with the plugin. |
| `ansible_paramiko_host_key_checking`, `ansible_ssh_host_key_checking`, `ansible_host_key_checking` | Whether the host key is checked, in that order, then `ANSIBLE_PARAMIKO_HOST_KEY_CHECKING`, `ANSIBLE_SSH_HOST_KEY_CHECKING`, `ANSIBLE_HOST_KEY_CHECKING`, `[paramiko_connection] host_key_checking` and `[defaults] host_key_checking`. Off, `ssh` gets `StrictHostKeyChecking=no` and `UserKnownHostsFile=/dev/null`. |
| `ansible_paramiko_use_rsa_sha2_algorithms` | Off, `ssh` gets `HostKeyAlgorithms=+ssh-rsa` and `PubkeyAcceptedAlgorithms=+ssh-rsa`, for a server that knows RSA keys only as `ssh-rsa`. `ssh` still prefers SHA-2 signatures with a server that offers them. Also `ANSIBLE_PARAMIKO_USE_RSA_SHA2_ALGORITHMS` and `[paramiko_connection] use_rsa_sha2_algorithms`. |
| `ansible_paramiko_proxy_command` | `ProxyCommand`, where `ssh` expands `%h`, `%p` and `%r` as paramiko does. Also `ANSIBLE_PARAMIKO_PROXY_COMMAND` and `[paramiko_connection] proxy_command`. |

The options with no variable come from `ANSIBLE_PARAMIKO_<NAME>` or `[paramiko_connection]`:

| Option | What `ssh` gets |
|---|---|
| `host_key_auto_add` | On, with host key checking on: `StrictHostKeyChecking=accept-new`. A new host's key is accepted, a changed key is still refused. |
| `record_host_keys` | Off, with host key checking on: `UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2`. `ssh` checks against the usual files and writes a new key to the first one, which keeps nothing. |
| `look_for_keys` | Off, with a key file given: `IdentitiesOnly=yes`, so only that key is tried. Without a key file it changes nothing. |
| `pty` | Off: no terminal, as `ansible_ssh_use_tty: false` does for `ssh`. |
| `banner_timeout` | Read and checked, with no effect: `ssh` has no such setting. |

What differs from the plugin:

- `~/.ssh/config` and `/etc/ssh/ssh_config` are not read, as with paramiko: every `ssh` gets `-F /dev/null`. The `ansible_ssh_common_args`, `ansible_ssh_extra_args` and `ansible_ssh_executable` variables are not read either.
- A host key that is not known, with host key checking on and `host_key_auto_add` off, makes the host unreachable with `ssh`'s message. The plugin asks on the terminal.
- The failure messages are `ssh`'s, for an unknown host key as for a refused login, and the host is unreachable in both cases, as with the plugin.
- With `look_for_keys` off and a key file given, the keys of `ssh-agent` are not offered either. Paramiko still offers them.
- The connection to a host is shared between tasks, as for `ssh`.

With `-vvvv` the first line is `ESTABLISH PARAMIKO SSH CONNECTION FOR USER: <user> on PORT <port> TO <address>`, then the `SSH: EXEC` lines of an `ssh` host.

## Connections only ansible-core has

Volant reaches a host one of two ways. For `ssh`, `local` and the connections listed above, it runs its agent on the host and sends it the tasks. For any other connection plugin the controller's ansible-core can load, no agent runs: each task that needs the host goes to Volant's Python helper, which loads the connection plugin, connects, runs the task's action plugin over it as `ansible-playbook` would, and closes the connection again. ansible-core reads every option of the plugin from the host's variables, `ansible.cfg` and the environment, as it always does.

Everything else about the run is unchanged: loops, `when`, `register`, `changed_when`, `failed_when`, `until` and `retries`, `ignore_errors`, `no_log`, `serial`, `delegate_to`, handlers, `async` with `poll: 0` and `async_status`, check mode and the recap. A task that runs on the controller, such as `debug` or `set_fact`, runs there as before.

- A plugin that cannot connect makes the host unreachable with the plugin's own message.
- The lines the plugin prints at `-vvv`, such as `<host> ESTABLISH ... CONNECTION`, are printed under the task at the same verbosity.
- `ansible_password`, the plugin's other secret variables and the become password are written `********` in every line and every result.
- No more helper processes run at once than `forks` allows.
- `become` takes the escalation method of the task, `sudo` or `su`.

Each task pays for starting the plugin's connection, as `ansible-playbook` does, so this route is slower per task than the agent's.

## Host variables

Volant reads these per host, from the inventory or from any other variable source. A test checks this list against the names the connection code reads, so it stays in sync with the engine.

| Variable | Effect |
| --- | --- |
| `ansible_connection` | `ssh` (default) or `local` |
| `ansible_host`, `ansible_ssh_host` | Address to connect to. Defaults to the inventory name. A value that starts with `-` or holds whitespace or a control character is refused before `ssh` runs. |
| `ansible_port`, `ansible_ssh_port` | `ssh -p`. A quoted number works as well as a bare one. |
| `ansible_user`, `ansible_ssh_user` | `ssh -l` |
| `ansible_password`, `ansible_ssh_pass`, `ansible_ssh_password` | The connection password, offered to `ssh` as described above. |
| `ansible_private_key_file`, `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`, `ansible_ssh_timeout` | Seconds `ssh` waits for the connection, `ssh -o ConnectTimeout`. |
| `ansible_ssh_executable` | The `ssh` program to run. Defaults to `ssh` from your `PATH`. |
| `ansible_ssh_common_args` | Extra `ssh` arguments, split the way a shell would split them |
| `ansible_ssh_extra_args` | The same, added after the common ones |
| `ansible_ssh_use_tty` | Run `raw` and `script` commands on a terminal, as `ssh -tt` does. On unless set to false. The default comes from [`usetty`](/docs/reference/configuration/#ssh-connection-usetty). |
| `ansible_remote_tmp` | Directory that holds the agent cache on that host |
| `ansible_become` | Escalate or not, overriding the `become` keyword |
| `ansible_become_user`, `ansible_sudo_user`, `ansible_su_user` | Target user for escalation |
| `ansible_become_method` | `sudo` or `su` |
| `ansible_become_password`, `ansible_become_pass`, `ansible_sudo_pass`, `ansible_su_pass` | Escalation password |
| `ansible_become_exe`, `ansible_sudo_exe`, `ansible_su_exe` | The escalation program |
| `ansible_become_flags`, `ansible_sudo_flags`, `ansible_su_flags` | Its flags |
| `ansible_su_prompt_l10n` | A list of prompts that `su` may print. It replaces the built-in ones. |

When a host sets a variable and an alias of it, the one Ansible lets win applies. The table lists them from strongest to weakest, with the keyword that sets the same thing:

| Setting | Variables | Keyword |
|---|---|---|
| Address | `ansible_ssh_host`, `ansible_host` | |
| Port | `ansible_ssh_port`, `ansible_port` | `port` |
| User | `ansible_ssh_user`, `ansible_user` | `remote_user` |
| Key file | `ansible_ssh_private_key_file`, `ansible_private_key_file` | |
| Timeout | `ansible_ssh_timeout`, `ansible_timeout` | |
| Password | `ansible_ssh_password`, `ansible_ssh_pass`, `ansible_password` | |
| Connection | `ansible_connection` | `connection` |
| Escalation user, program and flags | `ansible_sudo_*` or `ansible_su_*` (the method's own), then `ansible_become_*` | `become_user`, `become_exe`, `become_flags` |
| Escalation password | `ansible_sudo_pass` or `ansible_su_pass`, then `ansible_become_pass`, then `ansible_become_password` | |

A password variable beats `-k` and `--connection-password-file`, and a variable set to an empty string offers no password at all. A connection variable works at every level that sets variables: inventory, `group_vars`, a play's `vars`, a block, a task's `vars` and `-e`.

The keywords work on a play, a block and a task, and `-c` sets the connection for hosts that name none. The order is the one Ansible uses: a host variable such as `ansible_user` wins, then the task, the block and the play, then the command line and `ansible.cfg`. Facts a host reports about itself never choose its user, port or connection. A keyword written as a template is rendered once per task. These values fail the task: a user name that starts with `-` or holds a space, a quote or a control character, a port outside 1-65535, and a connection name with anything but letters, digits, `_` and `.`. A delegated task uses the task's `remote_user` and `port` unless the delegate sets its own `ansible_user` or `ansible_port`.

An unbalanced quote in `ansible_ssh_common_args` or `ansible_ssh_extra_args` fails the host instead of being dropped. That matters behind a bastion: silently discarding a `ProxyCommand` would connect straight to an address that may belong to a different machine.

The `ansible.cfg` settings that affect connections, such as `timeout`, `remote_user`, `host_key_checking` and `forks`, are listed in [Configuration](/docs/reference/configuration/).

## The agent cache

The agent is a small static program, about 1.2 MB for x86_64 Linux. Volant uploads it once per host and reuses it on every later run, so a host holds only the agent and, when Python modules ran, a payload cache.

It lives at `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`, so with the default `remote_tmp` it sits under `~/.ansible/tmp/` in a home directory. The version in the path is the controller's own, which is how an upgraded controller avoids running an older agent. A task that escalates runs its own agent as the `become_user`, cached in that user's home, so a host holds one cache per account the run acts as.

When Python modules have run, a second directory, `volant-blobs-<version>-<uid>`, sits beside the agent directory in the same `remote_tmp` and holds the module payloads Volant sent.

How the agent gets there depends on the target. Over `ssh`, and in a container that has a shell, the agent is fed to a shell script on the connection, which checks the directory and writes the file. In a container with no shell, the runtime's own copy command puts the agent in, and the agent places itself under the same checks; docker does this with `docker cp`.

To clear both, remove the two directories. Nothing else is left behind:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

[The agent on your hosts](/docs/internals/agent/#the-agent-cache) covers the upload, the repair of a broken cache and the permission checks.

## Persistent connections

A host keeps its connection to the agent for the whole run, across plays. Tasks travel over that one connection instead of opening a new one each time. If the connection dies between two plays, Volant reconnects once, and a second failure reports the host unreachable.

Some tasks add a connection:

- Escalation. A host that uses `become` holds a second connection, to an agent running as the target user. It is kept until the end of the play, so a play of escalated tasks starts one escalated agent, not one per task. A host holds at most one escalated connection at a time: escalating to a different user replaces it.
- Delegation. A delegated task runs over the delegate's own connection, with the delegate's settings. A host whose play delegates to three other hosts holds those three connections alongside its own.

## One connection per host

All the `ssh` runs Volant makes for one inventory host share a single OpenSSH connection, so only the first run pays for the key exchange and the authentication. The shared connection stays open for 30 seconds after its last session. OpenSSH does the work, so `~/.ssh/config`, `Match`, `Include`, `ProxyJump` and your ssh-agent apply exactly as they do for `ansible-playbook`.

Volant leaves sharing to you when your own `ssh` arguments or configuration already set `ControlMaster`, `ControlPath` or `ControlPersist` for the host. Set `[volant] ssh_control_master = false` or `VOLANT_SSH_CONTROL_MASTER=0` to turn it off, see [Configuration](/docs/reference/configuration/#volant-ssh-control-master).

Each connection costs the controller three file descriptors. A hard limit of 1024 open files stops a run at about 330 connections, or about 169 escalating hosts. [The agent on your hosts](/docs/internals/agent/#one-connection-per-host) has the socket location, the keepalive settings and the check to run before a wide run.

## Become

`become` runs tasks as another user through `sudo` or `su`.

```yaml
- hosts: web
  become: true
  tasks:
    - name: Install nginx
      apt:
        name: nginx
        state: present

    - name: Check the app as its own user
      command: /opt/app/bin/healthcheck
      become_user: app
```

### How it works

Volant does not wrap each task in `sudo`. It starts a second agent for the target user:

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` is `-n` or `-k -S -p ''`, depending on a probe described below. Volant adds `-H` only when no `become_flags` is set: a flag list replaces it. Tasks that do not escalate keep using the unprivileged agent. The agent itself knows nothing about escalation. [Decision record 0004](/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why.

The escalated agent is kept until the end of the play, so a play of escalated tasks pays for `sudo` once per host.

### Which setting wins

`become`, `become_user` and `become_method` are accepted on a play, a block and a task. Measured against Ansible:

1. A host's `ansible_become` and `ansible_become_user` variables beat both keywords, in either direction.
2. The task keyword beats the play keyword.
3. The `ansible.cfg` defaults and `-b`, `--become-user` and `--become-method` apply last.

`become_method` is `sudo` or `su`. Any other method is not supported yet, and so are the fully qualified spellings `ansible.builtin.sudo` and `ansible.builtin.su`. Volant stops before a task runs and names the method, rather than escalating through a mechanism you did not ask for. When a run escalates and the method comes from `--become-method`, `ansible.cfg` or `ansible_become_method`, the run ends with exit code 2 before any task.

`become_user` can be a template such as `{{ app_user }}`. One that changes per loop item is refused, because one batch escalates to one user.

`become_exe` and `become_flags` are read from the keyword and from `ansible_become_exe` and `ansible_become_flags`, the host variable winning. Both are split into words the way a shell splits them, and an unbalanced quote fails the task.

### The become password

The password comes from one of these:

- `-K` (`--ask-become-pass`), which turns terminal echo off on Unix.
- `--become-password-file`, which reads the first line of a file.
- `ansible_become_password`, `ansible_become_pass`, or the method's own `ansible_sudo_pass` or `ansible_su_pass`, which wins over the other two. [Host variables](#host-variables) lists which variable wins.

It is written once to `sudo`'s standard input. It never appears in a command line, a log or a result: the types that carry it print `<redacted>` in debug output, and `sudo`'s own error output is scrubbed of it before it reaches a message.

### The sudo probe

Volant asks `sudo` which form to use instead of assuming one. It tries `sudo -n` first, and falls back to `sudo -k -S -p ''` only when `sudo` itself rejects the request for lack of authentication. [Decision record 0004](/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explains why a guess would hang the link. On a host that ran before, Volant first starts the cached agent under `sudo -n` in the same `ssh` session as its checks, and runs the probe only when that does not work, as [The agent on your hosts](/docs/internals/agent/) describes.

When escalation fails, for a missing password, a wrong one or a `sudoers` rule that forbids the command, the task fails and the host stays reachable. The messages are Ansible's own: `Missing sudo password` or `Incorrect sudo password`.

### su

`su` has no flag to read a password without a terminal, so Volant answers its prompt instead. The password is written once to the prompt, never put in a command line, a log or `-vvv` output. A wrong password fails the task with `Incorrect su password`. A prompt that never comes ends the task with `Timeout (12s) waiting for privilege escalation prompt`, as in ansible-core.

Over a `local` connection `su` runs on a terminal of its own. A wrong password fails the task with `Premature end of stream waiting for become success.` and what `su` printed, as ansible-core does there. The wait for the prompt is always 12 seconds on a local connection, whatever `-T` says.

## Not there yet

- Windows hosts and network devices.
- The connection plugins other than `ssh` and `local`, listed in the [support matrix](/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` and the other become methods. They can be added as other ways to start the agent, without a protocol change.

[Keywords](/docs/reference/keywords/) is the full list, generated from the tables the engine reads.
