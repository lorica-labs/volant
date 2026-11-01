---
sourceHash: 8baaf4b329f9
title: 连接与 become
description: Volant 如何连接到主机、它读取的主机变量和 ansible.cfg 设置，以及 become 如何通过 sudo 和 su 工作。
---

本页回答 Volant 如何连接你的主机、哪些变量控制连接，以及 task 如何通过 `become` 以另一个用户的身份运行。

Volant 连接到受管主机，在那里放置一个小型静态 agent（运行你的 task 并回报结果的程序），并在运行的其余时间里通过同一个连接与它通信。

## 连接类型

`ansible_connection` 选择传输方式。与 Ansible 一样，默认值为 `ssh`。

| 值 | 作用 |
|---|---|
| `ssh` | 运行 `PATH` 中的 OpenSSH 客户端。Volant 自己构建命令行，不使用包装脚本，也不使用 Rust 的 SSH 库，所以你的 `~/.ssh/config` 会生效。 |
| `local` | 在控制节点上把 agent 作为子进程运行，不经过网络。 |
| `docker` | 通过 `docker exec -i` 在运行中的容器里运行 agent，与 `community.docker.docker` 相同。见 [Docker 容器](#docker-容器)。 |
| `podman` | 通过 `podman exec -i` 在运行中的 Podman 容器里运行 agent，无论是否 rootless，与 `containers.podman.podman` 相同。见 [Podman 容器](#podman-容器)。 |
| `chroot` | 通过 `chroot` 在控制节点上的目录树里运行 agent，与 `community.general.chroot` 相同。见 [chroot 目录](#chroot-目录)。 |
| `buildah` | 通过 `buildah run` 在 Buildah 工作容器里运行 agent，与 `containers.podman.buildah` 相同。见 [Buildah 工作容器](#buildah-工作容器)。 |
| `lxd` | 通过 `lxc exec` 在 LXD 实例里运行 agent，与 `community.general.lxd` 相同；`community.general.incus` 对 Incus 做同样的事。见 [LXD 和 Incus 实例](#lxd-和-incus-实例)。 |
| `kubectl` | 通过 `kubectl exec` 在 Kubernetes pod 的容器里运行 agent，与 `kubernetes.core.kubectl` 相同。见 [Kubernetes pod](#kubernetes-pod)。 |
| `aws_ssm` | 通过 AWS Systems Manager 会话在 EC2 实例上运行 agent，与 `amazon.aws.aws_ssm` 相同。见 [AWS Systems Manager 会话](#aws-systems-manager-会话)。 |
| `paramiko_ssh` | 用 OpenSSH 客户端代替 ansible-core 已弃用的 paramiko 连接；`paramiko` 也一样。见 [Paramiko](#paramiko)。 |

其他任何名字都由控制节点上的 ansible-core 解析，与 `ansible-playbook` 的解析方式相同，每个名字每次运行只解析一次。`ansible.builtin.ssh` 和 `ansible.builtin.local` 解析为上面的两个插件并运行。collection 从 ansible-core 手中接管的短名字，在该 collection 已安装时被转到那个 collection：

| 短名字 | 解析为 | `ansible` 12.3.0 包中的 collection |
|---|---|---|
| `docker` | `community.docker.docker` | community.docker 4.8.5 |
| `podman` | `containers.podman.podman` | containers.podman 1.18.0 |
| `buildah` | `containers.podman.buildah` | containers.podman 1.18.0 |
| `kubectl` | `kubernetes.core.kubectl` | kubernetes.core 6.2.0 |
| `lxd` | `community.general.lxd` | community.general 11.4.2 |
| `chroot` | `community.general.chroot` | community.general 11.4.2 |
| `aws_ssm` | `community.aws.aws_ssm`，`community.aws` 10.0.0 再把它转到 `amazon.aws.aws_ssm` | amazon.aws 10.1.2 |
| `incus` | 没有短名字：写 `community.general.incus` | community.general 11.4.2 |

使用 `-vvv` 时，被转向的名字会在某台主机上第一个使用它的 task 下打印 `redirecting (type: connection) ansible.builtin.chroot to community.general.chroot`，每一跳一行。

解析为 Volant 自己不驱动的插件的名字，例如 `winrm`、`psrp`、`community.general.jail`，或 playbook 旁 `connection_plugins/` 目录中的插件，会走第二条路线：见[只有 ansible-core 才有的连接](#只有-ansible-core-才有的连接)。如果没有控制器的 ansible-core 可问，这样的名字会让该主机不可达，消息为 `connection '<full name>' is not supported`，其余的运行继续。[支持矩阵](/zh-cn/docs/reference/parity/#collection-connection)列出了包中各个 collection 的全部连接插件。

什么都解析不到的名字会让 task 失败，并给出 `the connection plugin '<name>' was not found`，与 Ansible 一样。这包括被转到未安装 collection 的短名字，例如没有 community.docker 时的 `docker`（在 ansible-core 为它打印的警告之后），以及 `ansible.builtin.paramiko`，ansible-core 2.19.12 不以这个名字提供它。不论这个值是主机自己的连接、`delegate_to` 目标的连接，还是运行 `run_once` task 的那台主机的连接，都是如此。

密码来自 `-k`、`--connection-password-file` 或 `ansible_password` 变量。Volant 通过 `SSH_ASKPASS` 和一个运行结束时会删除的私有临时文件把它交给 `ssh`，这需要 OpenSSH 8.4 或更高版本。随后 `ssh` 以 `BatchMode=no` 和 `NumberOfPasswordPrompts=1` 运行，所以一次错误的回答就会结束这次尝试。密码绝不会出现在命令行、程序的环境或输出中，`-vvvv` 也不例外。用 vault 加密的 `ansible_password` 与任何 vault 加密的变量一样工作。没有密码时，`ssh` 以 `BatchMode=yes` 运行，从不提示，所以需要密码或口令的主机会被报告为不可达，而不是让运行卡住。

## Docker 容器

`ansible_connection: docker` 或 `community.docker.docker` 在运行中的容器里执行 play，容器里不需要 sshd，也不需要到它的网络路径。两个名字都需要控制节点上装有 community.docker，与 `ansible-playbook` 一样。Volant 运行你 `PATH` 中的 `docker` 程序，因此 `DOCKER_HOST`、`DOCKER_CONTEXT` 和 `DOCKER_CONFIG` 通过环境传给它。

agent 通过 `docker exec -i` 送进去，所以镜像需要 `/bin/sh` 和常用工具：`uname`、`id`、`ls`、`mkdir`、`df`、`awk`、`head`、`wc`、`chmod`、`mv` 和 `find`。最小的 Alpine 或 Debian 镜像都有。没有 shell 的镜像，例如 distroless，改用 `docker cp` 获得 agent：Volant 向 `docker inspect` 询问镜像的架构，把 agent 复制到容器的 `/tmp`，再由 agent 按同样的检查把自己放进缓存目录。容器需要有 `/tmp`，`remote_tmp` 必须是绝对路径，或者对有主目录的用户以 `~` 开头，并且 `become` 会被拒绝，因为没有 shell 来运行 `sudo`。容器的用户无法删除时，`/tmp` 中的副本会留下。

| 变量 | 作用 |
|---|---|
| `ansible_docker_host` | 容器的名字或 id。它优先于 `ansible_host`，后者优先于清单名。 |
| `ansible_docker_user` | 命令以哪个用户运行，即 `docker exec -u`。它优先于 `ansible_user`、`remote_user` 关键字和 `-u`。未设置时用容器自己的用户。 |
| `ansible_docker_extra_args` | 放在 `docker` 之后、`exec` 之前的参数，按单个空格切分，而不是按 shell 词切分。默认值来自 `[docker_connection] extra_cli_args`。 |
| `ansible_docker_extra_env` | 用 `docker exec -e` 设置的变量映射。每个值都必须是文本：不带引号的数字或布尔值会被拒绝。 |
| `ansible_docker_privileged` | `docker exec --privileged`。默认值来自 `ANSIBLE_DOCKER_PRIVILEGED` 或 `[docker_connection] privileged`。 |
| `ansible_docker_working_dir` | `docker exec -w`。默认值来自 `ANSIBLE_DOCKER_WORKING_DIR` 或 `[docker_connection] working_dir`。 |
| `ansible_docker_timeout` | `docker` 回应第一条命令可用的秒数。默认值依次来自 `ANSIBLE_DOCKER_TIMEOUT`、`[docker_connection] timeout`，再到连接超时。 |

`become` 配合 `sudo` 时使用容器自己的 `sudo`。`su` 会被拒绝，因为 `docker exec` 不给它终端。容器不存在或已停止，或镜像里没有该用户时，主机不可达，消息是 docker 自己的。ansible-core 把这三种情况都报告为 `Failed to create temporary directory`，因此这条消息是有意不同的。

使用 `-vvvv` 时，会显示 `ESTABLISH DOCKER CONNECTION FOR USER: <user>`，然后每条命令一行 `EXEC [...]`，这些是 Volant 自己的命令。play 没有指定用户时，用户来自 `docker inspect`：镜像没有指定时为 `root`，docker 无法给出时为 `?`。

## Podman 容器

`ansible_connection: podman` 或 `containers.podman.podman` 在 Podman 容器里执行 play，无论是否 rootless，不需要守护进程，也不需要 ssh。两个名字都需要控制节点上装有 containers.podman。agent 的送入方式与 docker 相同：镜像有 shell 时通过 `podman exec -i`，没有时通过 `podman cp`。

Volant 按插件源码的方式读取选项，这与它的文档不同。容器是 `ansible_host`，否则是清单名；文档中的 `ansible_podman_host` 不会被读取。用户是 `ansible_user`、`remote_user` 关键字或 `-u`，以 `podman exec --user` 传入。

| 变量 | 作用 |
|---|---|
| `ansible_podman_executable` | `podman` 程序，可以是在你的 `PATH` 中查找的名字，也可以是路径。默认值来自 `ANSIBLE_PODMAN_EXECUTABLE`。 |
| `ansible_podman_extra_args` | 放在 `podman` 之后、子命令之前的参数，按 shell 的方式切分。默认值来自 `ANSIBLE_PODMAN_EXTRA_ARGS` 或 `[defaults] podman_extra_args`。 |

插件运行 `podman exec` 时不带 `-i`，因此无法把 stdin 交给命令。agent 需要 stdin，所以 Volant 运行 `podman exec -i`；这是它的命令行唯一多出的词。Volant 从不运行 `podman mount`。`become` 配合 `sudo` 时使用容器自己的 `sudo`，`su` 会被拒绝。使用 `-vvvv` 时，每条命令显示为一行 `RUN [...]`，与插件的写法相同，没有 `ESTABLISH` 行。

## chroot 目录

`ansible_connection: chroot` 或 `community.general.chroot` 在控制节点上的目录树里执行 play：构建镜像、`debootstrap` 目录树、救援操作。清单名或 `ansible_host` 就是该目录。两个名字都需要控制节点上装有 community.general。运行需要 root，因为 `chroot` 需要。

| 变量 | 作用 |
|---|---|
| `ansible_chroot_exe` | `chroot` 程序，可以是在你的 `PATH` 中查找的名字，也可以是绝对路径。默认值来自 `ANSIBLE_CHROOT_EXE` 或 `[chroot_connection] exe`。 |
| `ansible_chroot_disable_root_check` | 跳过控制节点是否以 root 运行的检查。默认值来自 `ANSIBLE_CHROOT_DISABLE_ROOT_CHECK` 或 `[chroot_connection] disable_root_check`。 |
| `ansible_executable` | 命令运行所用的 shell，默认为 `/bin/sh`。默认值来自 `ANSIBLE_EXECUTABLE` 或 `[defaults] executable`。 |

在任何东西运行之前，以下情况会让 task 失败，与 ansible-core 相同：路径不是目录，目录树没有 `bin/sh`，以及控制节点不是 root（除非关闭了该检查）。agent 通过 `chroot <dir> /bin/sh` 送入，所以写入它的脚本在目录树里运行，无论目录树中有什么符号链接，它写的路径都不会离开目录树。当 `df` 无法给出剩余空间时，例如目录树里没有挂载 `/proc`，会跳过空间检查。目录树里有 `sudo` 时可以使用 `become`。使用 `-vvvv` 时，会显示 `THIS IS A LOCAL CHROOT DIR`，然后每条命令一行 `EXEC [...]`。

## Buildah 工作容器

`ansible_connection: buildah` 或 `containers.podman.buildah` 配置 `buildah from <镜像>` 创建的工作容器，让 play 逐层构建镜像，之后由 `buildah commit` 保存。两个名字都需要控制节点上装有 containers.podman。与 podman 一样，容器是 `ansible_host`，否则是清单名；用户是 `ansible_user`、`remote_user` 关键字或 `-u`；`root` 不会加任何选项。程序是你 `PATH` 中的 `buildah`，`BUILDAH_*` 变量通过环境传给它。

agent 用 `buildah run [--user U] <容器> -- <agent>` 启动。Volant 从不运行 `buildah mount`，因为它对 rootless 用户需要 `buildah unshare`：没有 shell 的工作容器通过 `buildah copy` 获得 agent。rootless 的 buildah 每条命令都会输出关于 capability 的警告，因此不显示 agent 的 stderr。使用 `-vvvv` 时，每条命令显示为一行 `RUN [...]`，没有 `ESTABLISH` 行。

## LXD 和 Incus 实例

`ansible_connection: lxd` 或 `community.general.lxd` 通过 `lxc` 命令在 LXD 实例里执行 play，实例可以在本地，也可以在远程服务器上。`community.general.incus` 用 `incus` 命令对 Incus 做同样的事；它在 ansible-core 中没有短名字。两者都需要控制节点上装有 community.general。

| 变量 | 作用 |
|---|---|
| `ansible_lxd_host`、`ansible_incus_host` | 实例名，取到第一个点为止：`web.example.com` 即 `web`。它优先于 `ansible_host`，后者优先于清单名。 |
| `ansible_lxd_remote`、`ansible_incus_remote` | 远程服务器，默认为 `local`。 |
| `ansible_lxd_project`、`ansible_incus_project` | 项目。Incus 总会传一个，默认为 `default`。 |
| `ansible_lxd_executable`、`ansible_incus_executable` | 命令运行所用的 shell。它优先于 `ansible_executable`；默认为 `/bin/sh`。 |

用户是 `ansible_user`、`remote_user` 关键字或 `-u`，默认为 `root`。对其他任何用户，每条命令都通过提权方法执行，默认是 `/bin/su <user> -c`，除非 `lxd_become_method` 或 `incus_become_method` 指定了别的方法。用户为 root 时，没有 shell 的实例通过 `lxc file push` 或 `incus file push` 获得 agent。实例不存在或已停止时，主机不可达，消息使用插件的措辞，例如 `instance not running: web`；Incus 还会加上远程服务器和项目。使用 `-vvvv` 时，会显示 `ESTABLISH LXD CONNECTION FOR USER: <user>`（Incus 为 `Incus`），对非 root 用户还有插件的 `INFO:` 行，然后每条命令一行 `EXEC [...]`。

## Kubernetes pod

`ansible_connection: kubectl` 或 `kubernetes.core.kubectl` 通过 `kubectl exec -i` 在 pod 的容器里执行 play。两个名字都需要控制节点上装有 kubernetes.core，并且 `kubectl` 在 `PATH` 中。

| 变量 | 作用 |
|---|---|
| `ansible_kubectl_pod` | pod。未设置时，pod 为 `ansible_host`，否则为清单名。 |
| `ansible_kubectl_container` | 容器，用于有多个容器的 pod。 |
| `ansible_kubectl_namespace` | namespace。 |
| `ansible_kubectl_kubeconfig`、`ansible_kubectl_config` | kubeconfig 文件。以映射给出的 kubeconfig 会写入一个私有临时文件，运行结束时删除。 |
| `ansible_kubectl_extra_args` | 传给 `kubectl` 的额外参数，按空格拆分。 |
| `ansible_kubectl_local_env_vars` | 一个变量映射，设置到每条 `kubectl` 命令的环境中。 |
| `ansible_kubectl_context` | kubeconfig 中的 context。 |
| `ansible_kubectl_server`、`ansible_kubectl_host` | API 服务器。 |
| `ansible_kubectl_username`、`ansible_kubectl_user`、`ansible_kubectl_password` | 访问 API 服务器的用户名和密码。 |
| `ansible_kubectl_token`、`ansible_kubectl_api_key` | 访问 API 服务器的 bearer token。 |
| `ansible_kubectl_client_cert`、`ansible_kubectl_cert_file`、`ansible_kubectl_client_key`、`ansible_kubectl_key_file` | 客户端证书及其私钥。 |
| `ansible_kubectl_ca_cert`、`ansible_kubectl_ssl_ca_cert` | API 服务器的证书颁发机构。 |
| `ansible_kubectl_validate_certs`、`ansible_kubectl_verify_ssl` | 与插件一样，原样放在 `--insecure-skip-tls-verify` 之后：`true` 得到 `True`，`false` 则不添加任何内容。 |

每个选项也可以来自插件读取的 `K8S_AUTH_*` 环境变量，主机变量优先于它。参数按插件的顺序传给 `kubectl`。命令以容器的用户运行：插件无法选择用户，因此 `ansible_user` 会被忽略。没有 shell 的容器无法接收 agent，因为 `kubectl cp` 需要容器里有 `tar`。pod 不存在、namespace 不存在或 pod 已结束时，主机不可达，消息为 `kubectl` 的输出。使用 `-vvvv` 时，会显示 `ESTABLISH kubectl CONNECTION`，然后每条命令一行 `EXEC [...]`，其中密码和 token 显示为 `********`。

## AWS Systems Manager 会话

`ansible_connection: amazon.aws.aws_ssm` 通过 AWS Systems Manager Session Manager 在 EC2 实例上执行 play，不需要入站端口、密钥对或堡垒机。短名字 `aws_ssm` 经由 community.aws 指向它，因此也需要安装该集合。控制节点需要 `session-manager-plugin`，其 Python 中需要 boto3；实例需要 SSM agent。

| 变量 | 作用 |
|---|---|
| `ansible_aws_ssm_instance_id` | 实例。未设置时，实例为 `ansible_host`，否则为清单名。 |
| `ansible_aws_ssm_region` | 区域，否则为 `AWS_DEFAULT_REGION` 或 `AWS_REGION`；默认为 `us-east-1`。 |
| `ansible_aws_ssm_profile` | AWS 配置文件，否则为 `AWS_PROFILE`。 |
| `ansible_aws_ssm_access_key_id`、`ansible_aws_ssm_secret_access_key`、`ansible_aws_ssm_session_token` | 凭据，否则为同名的 `AWS_*` 环境变量，再否则为 boto3 自己找到的凭据。它们从不显示，`-vvvv` 也不例外。 |
| `ansible_aws_ssm_plugin` | `session-manager-plugin` 程序，否则为 `AWS_SESSION_MANAGER_PLUGIN`、`/usr/local/bin/session-manager-plugin` 或 `PATH` 中的那个。 |
| `ansible_aws_ssm_document` | 会话启动的 SSM 文档；未设置时为账户的默认 shell。 |
| `ansible_aws_ssm_timeout` | 会话在建立连接的每一步中可用的秒数；默认为 60。 |

命令以会话的用户运行，在 Amazon Linux 上是 `ssm-user`。`become` 可以选择其他用户，`ansible_user` 会被忽略，与插件相同。Volant 不通过 S3 传输任何内容，因此插件需要的 `ansible_aws_ssm_bucket_*` 变量不会被读取。会话的终端只返回有效的 UTF-8，因此 agent 用 base64 行与控制节点通信，每个方向约 400 KiB/s；在某个实例上首次运行时会上传一次 agent，约需十秒。SSM agent 未管理的实例或被拒绝的凭据会使主机不可达，消息使用 boto3 的措辞，而插件在这种情况下会使任务失败。运行结束或控制节点被杀死时，会话关闭，agent 随之停止。使用 `-vvvv` 时，会显示 `ESTABLISH SSM CONNECTION TO: <instance>`，然后每条命令一行 `EXEC: <command>`。

## Paramiko

`ansible_connection: paramiko_ssh` 是 ansible-core 通过 Python paramiko 库建立的连接。ansible-core 2.19.12 把它标为弃用，并在 2.21 移除。Volant 改用 `ssh` 程序连接这些主机，所以不需要安装 paramiko。插件的每个选项都变成最接近的 `ssh` 选项，第一台连接的主机会打印 ansible-core 的警告，每次运行一次：

```text
[DEPRECATION WARNING]: The paramiko connection plugin is deprecated. This feature will be removed from ansible-core version 2.21.
```

`deprecation_warnings = False` 可以关掉它。`paramiko`、`_paramiko_ssh` 和 `ansible.builtin.paramiko_ssh` 是同一个插件。

| 变量 | 作用 |
|---|---|
| `ansible_paramiko_host` | 地址。它优先于 `ansible_ssh_host`，后者优先于 `ansible_host`，再往后是 inventory 名字。 |
| `ansible_paramiko_port` | `ssh -p`。优先于 `ansible_ssh_port` 和 `ansible_port`，再往后是 `port` 关键字、`ANSIBLE_REMOTE_PARAMIKO_PORT`、`ANSIBLE_REMOTE_PORT`、`[paramiko_connection] remote_port` 和 `[defaults] remote_port`。 |
| `ansible_paramiko_user` | `ssh -l`。优先于 `ansible_ssh_user` 和 `ansible_user`，再往后是 `remote_user` 关键字、`ANSIBLE_PARAMIKO_REMOTE_USER`、`[paramiko_connection] remote_user` 和常规的远程用户设置。 |
| `ansible_paramiko_private_key_file` | `ssh -i`。优先于 `ansible_ssh_private_key_file` 和 `ansible_private_key_file`，再往后是 `ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE`、`[paramiko_connection] private_key_file` 和常规的密钥设置。 |
| `ansible_paramiko_password`, `ansible_paramiko_pass` | 密码，按 `ssh` 主机的方式交给 `ssh`。它们优先于 `ansible_ssh_password`、`ansible_ssh_pass` 和 `ansible_password`。 |
| `ansible_paramiko_timeout` | `ConnectTimeout`。优先于 `ansible_ssh_timeout`，再往后是 `ANSIBLE_PARAMIKO_TIMEOUT`、`ANSIBLE_SSH_TIMEOUT`、`[paramiko_connection] timeout`、`[ssh_connection] timeout` 和连接超时。与插件一样，不读 `ansible_timeout`。 |
| `ansible_paramiko_host_key_checking`, `ansible_ssh_host_key_checking`, `ansible_host_key_checking` | 是否检查主机密钥，按这个顺序，再往后是 `ANSIBLE_PARAMIKO_HOST_KEY_CHECKING`、`ANSIBLE_SSH_HOST_KEY_CHECKING`、`ANSIBLE_HOST_KEY_CHECKING`、`[paramiko_connection] host_key_checking` 和 `[defaults] host_key_checking`。关闭时 `ssh` 得到 `StrictHostKeyChecking=no` 和 `UserKnownHostsFile=/dev/null`。 |
| `ansible_paramiko_use_rsa_sha2_algorithms` | 关闭时 `ssh` 得到 `HostKeyAlgorithms=+ssh-rsa` 和 `PubkeyAcceptedAlgorithms=+ssh-rsa`，用于只把 RSA 密钥认作 `ssh-rsa` 的服务器。服务器提供 SHA-2 签名时，`ssh` 仍然优先用它们。也可以用 `ANSIBLE_PARAMIKO_USE_RSA_SHA2_ALGORITHMS` 和 `[paramiko_connection] use_rsa_sha2_algorithms`。 |
| `ansible_paramiko_proxy_command` | `ProxyCommand`，`ssh` 会像 paramiko 一样替换 `%h`、`%p` 和 `%r`。也可以用 `ANSIBLE_PARAMIKO_PROXY_COMMAND` 和 `[paramiko_connection] proxy_command`。 |

没有对应变量的选项来自 `ANSIBLE_PARAMIKO_<NAME>` 或 `[paramiko_connection]`：

| 选项 | `ssh` 得到什么 |
|---|---|
| `host_key_auto_add` | 打开且主机密钥检查打开时：`StrictHostKeyChecking=accept-new`。新主机的密钥会被接受，变化了的密钥仍被拒绝。 |
| `record_host_keys` | 关闭且主机密钥检查打开时：`UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2`。`ssh` 对照常用文件检查，并把新密钥写进第一个文件，而它什么都不保存。 |
| `look_for_keys` | 关闭且给了密钥文件时：`IdentitiesOnly=yes`，只尝试这个密钥。没给密钥文件时不起作用。 |
| `pty` | 关闭时：没有终端，与 `ssh` 的 `ansible_ssh_use_tty: false` 一样。 |
| `banner_timeout` | 会读取并检查，但不起作用：`ssh` 没有这个设置。 |

与插件不同的地方：

- 与 paramiko 一样，不读 `~/.ssh/config` 和 `/etc/ssh/ssh_config`：每次 `ssh` 都带 `-F /dev/null`。`ansible_ssh_common_args`、`ansible_ssh_extra_args` 和 `ansible_ssh_executable` 也不读。
- 主机密钥检查打开、`host_key_auto_add` 关闭时，未知的主机密钥会让主机不可达，并给出 `ssh` 的消息。插件会在终端上询问。
- 失败消息都是 `ssh` 的，未知主机密钥和登录被拒都是如此，两种情况下主机都不可达，与插件一样。
- `look_for_keys` 关闭且给了密钥文件时，`ssh-agent` 里的密钥也不会提供。paramiko 仍会提供它们。
- 与 `ssh` 一样，到一台主机的连接在 task 之间共享。

使用 `-vvvv` 时，第一行是 `ESTABLISH PARAMIKO SSH CONNECTION FOR USER: <user> on PORT <port> TO <address>`，后面是 `ssh` 主机的 `SSH: EXEC` 行。

## 只有 ansible-core 才有的连接

Volant 用两种方式之一到达主机。对于 `ssh`、`local` 和上面列出的连接，它在主机上运行自己的 agent 并把 task 发给它。对于控制器的 ansible-core 能加载的其他任何连接插件，不运行 agent：每个需要主机的 task 都交给 Volant 的 Python 辅助进程，由它加载连接插件、建立连接、像 `ansible-playbook` 那样通过它运行 task 的 action 插件，task 结束后关闭连接。循环的各个 item 共用一个连接，与 `ansible-playbook` 相同。ansible-core 照常从主机变量、`ansible.cfg` 和环境中读取插件的每个选项。

运行的其他部分都不变：循环、`when`、`register`、`changed_when`、`failed_when`、`until` 和 `retries`、`ignore_errors`、`no_log`、`serial`、`delegate_to`、handler、带 `poll: 0` 的 `async` 和 `async_status`、检查模式以及汇总。在控制器上运行的 task，例如 `debug` 或 `set_fact`，照旧在控制器上运行。

- 无法连接的插件会让主机不可达，并给出插件自己的消息。
- 插件在 `-vvv` 下打印的行，例如 `<host> ESTABLISH ... CONNECTION`，会以相同的详细程度打印在 task 下。
- `ansible_password`、插件的其他机密变量和 become 密码在每一行和每个结果中都写成 `********`。
- 同时运行的辅助进程数量不会超过 `forks` 允许的数量。
- `become` 使用 task 的提权方法，`sudo` 或 `su`，无论它写在 task、所在的 block 还是 play 上。写成模板时，例如 `become: "{{ item }}"`，会为循环的每个元素分别渲染。

每个 task 都要付出建立插件连接的代价，与 `ansible-playbook` 一样，因此这条路线每个 task 比 agent 的路线慢。

### 保留进程的连接

`network_cli`、`netconf` 和 `httpapi` 像在 `ansible-playbook` 下一样，把与设备的连接保存在单独的进程里。在第一个需要该设备的任务之前，helper 用自己的 Python、插件路径和详细级别运行 ansible-core 的 `ansible-connection` stub。stub 启动该进程，进程在 `~/.ansible/pc` 下的一个 socket 上监听，该目录由 `ANSIBLE_PERSISTENT_CONTROL_PATH_DIR` 或 `[persistent_connection] control_path_dir` 设定。socket 的名字是地址、端口、用户和本次运行的哈希，所以之后同一设备的每个任务都会复用该进程，无论来自哪个 helper。

- 无法启动的进程会让任务带着它的错误失败，而不是让主机失败。`-vvv` 会加上它的 traceback。
- `meta: reset_connection` 结束该主机的进程，每个 play 结束时也会结束它的任务启动的所有进程，与 `ansible-playbook` 一致。空闲 `persistent_connect_timeout` 秒（默认 30）的进程会自行结束，一条命令最多等待 `persistent_command_timeout` 秒（30）。
- 在 `-vvvv` 下，ansible-core 启动进程时打印的行，例如 `<address> attempting to start connection` 和 `<address> local domain socket path is ...`，会显示在任务下方。
- `persistent_log_messages` 会记录与设备的每次交互，包括密码，ansible-core 会用它自己的文字发出警告。

## 网络设备

`ansible_connection: network_cli`、`netconf` 或 `httpapi`，或它们在 `ansible.netcommon` 下的完整名称，通过 ansible.netcommon 自己的插件连接交换机、路由器或防火墙，这些插件由 ansible-core 在 Volant 的 Python helper 中加载。控制端需要安装 ansible.netcommon 和 ansible.utils，以及设备平台的 collection，例如 `cisco.ios`。`ansible_network_os` 指明该平台（`cisco.ios.ios`）。没有它的 `network_cli` 主机会以 `Unable to automatically determine host network os` 变为不可达。

网络 collection 的模块，例如 `ansible.netcommon.cli_command` 或 `cisco.ios.ios_config`，像在 `ansible-playbook` 下一样在控制端运行，并通过连接的持久进程访问设备。需要与主机不同连接的模块会以其 collection 自己的消息失败，例如 `Connection type local is not valid for this module`。

每种连接都需要 ansible-core 不自带的 Python 包：`network_cli` 需要 paramiko 或 ansible-pylibssh，`netconf` 需要 ncclient。使用 Volant 的托管运行时时，`volant runtime install --extras network` 会安装它们；缺少它们时，需要它们的运行会在第一次连接之前停止。使用 `VOLANT_PYTHON` 时，请在该环境中安装它们。

插件自己从主机变量、`ansible.cfg` 和环境中读取选项，就像 `ansible-doc -t connection ansible.netcommon.network_cli` 所列出的那样。

## 主机变量

Volant 为每台主机从 inventory 或任何其他变量来源读取这些变量。有一个测试会把这份列表与连接代码读取的名字对照检查，所以它与引擎保持同步。

| 变量 | 作用 |
| --- | --- |
| `ansible_connection` | `ssh`（默认）或 `local` |
| `ansible_host`、`ansible_ssh_host` | 要连接的地址。默认为 inventory 中的名字。以 `-` 开头，或含有空白或控制字符的值，会在 `ssh` 运行之前被拒绝。 |
| `ansible_port`、`ansible_ssh_port` | `ssh -p`。带引号的数字和裸数字都可以。 |
| `ansible_user`、`ansible_ssh_user` | `ssh -l` |
| `ansible_password`、`ansible_ssh_pass`、`ansible_ssh_password` | 连接密码，按上文所述提供给 `ssh`。 |
| `ansible_private_key_file`、`ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`、`ansible_ssh_timeout` | `ssh` 等待连接的秒数，即 `ssh -o ConnectTimeout`。 |
| `ansible_ssh_executable` | 要运行的 `ssh` 程序。默认为 `PATH` 中的 `ssh`。 |
| `ansible_ssh_common_args` | 额外的 `ssh` 参数，按 shell 的方式拆分 |
| `ansible_ssh_extra_args` | 同上，追加在通用参数之后 |
| `ansible_ssh_use_tty` | 在终端上运行 `raw` 和 `script` 命令，就像 `ssh -tt` 那样。除非设为 false，否则开启。默认值来自 [`usetty`](/zh-cn/docs/reference/configuration/#ssh-connection-usetty)。 |
| `ansible_remote_tmp` | 该主机上存放 agent 缓存的目录 |
| `ansible_become` | 是否提权，覆盖 `become` 关键字 |
| `ansible_become_user`、`ansible_sudo_user`、`ansible_su_user` | 提权的目标用户 |
| `ansible_become_method` | `sudo` 或 `su` |
| `ansible_become_password`、`ansible_become_pass`、`ansible_sudo_pass`、`ansible_su_pass` | 提权密码 |
| `ansible_become_exe`、`ansible_sudo_exe`、`ansible_su_exe` | 提权所用的程序 |
| `ansible_become_flags`、`ansible_sudo_flags`、`ansible_su_flags` | 它的标志 |
| `ansible_su_prompt_l10n` | `su` 可能打印的提示列表，或用逗号分隔这些提示的字符串。它会替换内置的提示。 |
| `ansible_sudo_chdir` | 启动 `sudo` 时所在的目录。目录不存在时 task 会失败。 |

当一台主机同时设置了某个变量和它的别名时，生效的是 Ansible 让其获胜的那个。下表按从强到弱的顺序列出它们，以及设置同一内容的关键字：

| 设置 | 变量 | 关键字 |
|---|---|---|
| 地址 | `ansible_ssh_host`、`ansible_host` | |
| 端口 | `ansible_ssh_port`、`ansible_port` | `port` |
| 用户 | `ansible_ssh_user`、`ansible_user` | `remote_user` |
| 密钥文件 | `ansible_ssh_private_key_file`、`ansible_private_key_file` | |
| 超时 | `ansible_ssh_timeout`、`ansible_timeout` | |
| 密码 | `ansible_ssh_password`、`ansible_ssh_pass`、`ansible_password` | |
| 连接 | `ansible_connection` | `connection` |
| 提权的用户、程序和标志 | `ansible_sudo_*` 或 `ansible_su_*`（该方法自己的），然后是 `ansible_become_*` | `become_user`、`become_exe`、`become_flags` |
| 提权密码 | `ansible_sudo_pass` 或 `ansible_su_pass`，然后是 `ansible_become_pass`，再然后是 `ansible_become_password` | |

密码变量优先于 `-k` 和 `--connection-password-file`，被设为空字符串的变量则完全不提供密码。连接变量在每个能设置变量的层级都有效：inventory、`group_vars`、play 的 `vars`、block、task 的 `vars` 以及 `-e`。

这些关键字可用于 play、block 和 task，`-c` 为没有指定连接的主机设置连接。顺序与 Ansible 相同：先是 `ansible_user` 这样的主机变量，然后是 task、block 和 play，最后是命令行和 `ansible.cfg`。主机报告的关于自身的 facts 绝不会决定它的用户、端口或连接。写成模板的关键字，每个 task 渲染一次。下列值会让 task 失败：以 `-` 开头或含有空格、引号或控制字符的用户名，超出 1-65535 的端口，以及含有字母、数字、`_` 和 `.` 以外字符的连接名。被委派的 task 使用该 task 的 `remote_user` 和 `port`，除非委派目标自己设置了 `ansible_user` 或 `ansible_port`。

`ansible_ssh_common_args` 或 `ansible_ssh_extra_args` 中引号不配对会让该主机失败，而不是被丢弃。这在堡垒机后面很重要：悄悄丢掉 `ProxyCommand` 会直接连接到一个可能属于另一台机器的地址。

影响连接的 `ansible.cfg` 设置，例如 `timeout`、`remote_user`、`host_key_checking` 和 `forks`，列在[配置](/zh-cn/docs/reference/configuration/)中。

## agent 缓存

agent 是一个小型静态程序，x86_64 Linux 版本约 1.2 MB。Volant 为每台主机只上传一次，并在之后的每次运行中复用，所以主机上只保留 agent，以及在运行过 Python 模块时的 payload 缓存。

它位于 `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`，所以使用默认的 `remote_tmp` 时，它位于主目录中的 `~/.ansible/tmp/` 之下。路径中的版本是控制节点自己的版本，这就是升级后的控制节点避免运行旧版 agent 的方式。提权的 task 会以 `become_user` 的身份运行它自己的 agent，缓存在该用户的主目录中，所以运行所涉及的每个账户，主机上都有一份缓存。

运行过 Python 模块后，第二个目录 `volant-blobs-<version>-<uid>` 会位于同一个 `remote_tmp` 中、agent 目录的旁边，保存 Volant 发送的模块 payload。

agent 如何到达取决于目标。通过 `ssh`，以及在有 shell 的容器里，agent 会在连接上交给一个 shell 脚本，由它检查目录并写入文件。在没有 shell 的容器里，由运行时自己的复制命令把 agent 放进去，再由 agent 按同样的检查放置自己；docker 用 `docker cp` 完成这一步。

要清除两者，删除这两个目录即可。不会留下任何其他东西：

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

上传、损坏缓存的修复和权限检查见[主机上的 agent](/zh-cn/docs/internals/agent/#agent-缓存)。

## 持久连接

一台主机在整个运行期间（跨越多个 play）保持与 agent 的连接。task 通过这一个连接传输，而不是每次都打开新连接。如果连接在两个 play 之间断开，Volant 会重新连接一次，第二次失败则把该主机报告为不可达。

通过 `local` 连接的主机则共享它们的 agent。这些 agent 都以同一用户在控制节点上运行，所以等待其他主机的主机会把自己的 agent 交给下一个运行的主机，整个运行最多保留 `forks` 个空闲的 agent。在 500 个本地主机上运行的 play 保留的 agent 数量大约与 `forks` 允许的一样多，而不是 500 个。提权的本地 agent 仍归它的主机所有。

有些 task 会增加连接：

- 提权。使用 `become` 的主机会持有第二个连接，连到以目标用户身份运行的 agent。它会保持到 play 结束，所以由提权 task 组成的 play 只会启动一个提权 agent，而不是每个 task 启动一个。一台主机同一时间最多持有一个提权连接：提权到另一个用户会替换它。
- 委派。被委派的 task 通过委派目标自己的连接运行，使用委派目标的设置。一台主机的 play 委派给另外三台主机时，除了自己的连接，还会持有这三个连接。

## 每台主机一个连接

Volant 为同一台 inventory 主机发起的所有 `ssh` 运行，共享同一个 OpenSSH 连接，所以只有第一次运行需要付出密钥交换和认证的代价。共享连接会在最后一个会话结束后保持打开 30 秒。工作由 OpenSSH 完成，所以 `~/.ssh/config`、`Match`、`Include`、`ProxyJump` 和你的 ssh-agent 的生效方式，与它们在 `ansible-playbook` 下完全相同。

当你自己的 `ssh` 参数或配置已经为该主机设置了 `ControlMaster`、`ControlPath` 或 `ControlPersist` 时，Volant 会把共享交给你。设置 `[volant] ssh_control_master = false` 或 `VOLANT_SSH_CONTROL_MASTER=0` 可以关闭它，见[配置](/zh-cn/docs/reference/configuration/#volant-ssh-control-master)。

每个连接会占用控制节点三个文件描述符。1024 个打开文件的硬限制会让运行在约 330 个连接、或约 169 台提权主机处停止。socket 的位置、保活设置，以及大规模运行之前要做的检查，见[主机上的 agent](/zh-cn/docs/internals/agent/#每台主机一个连接)。

## Become

`become` 通过 `sudo` 或 `su` 以另一个用户的身份运行 task。

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

### 工作原理

Volant 不会把每个 task 包在 `sudo` 里。它为目标用户启动第二个 agent：

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` 是 `-n` 或 `-k -S -p ''`，取决于下文所述的一次探测。只有在没有设置 `become_flags` 时，Volant 才会添加 `-H`：一旦给出标志列表，它就会取而代之。不提权的 task 继续使用无特权的 agent。agent 本身对提权一无所知。原因见[决策记录 0004](/zh-cn/docs/decisions/0004-become-runs-a-second-agent-under-sudo/)。

提权 agent 会保持到 play 结束，所以由提权 task 组成的 play，每台主机只需为 `sudo` 付出一次代价。

### 哪个设置优先

`become`、`become_user` 和 `become_method` 可用于 play、block 和 task。对照 Ansible 测得的结果：

1. 主机的 `ansible_become` 和 `ansible_become_user` 变量优先于这两个关键字，两个方向都是如此。
2. task 的关键字优先于 play 的关键字。
3. `ansible.cfg` 的默认值以及 `-b`、`--become-user` 和 `--become-method` 最后生效。

`become_method` 是 `sudo` 或 `su`。其他任何方法目前都不支持，完全限定写法 `ansible.builtin.sudo` 和 `ansible.builtin.su` 也一样。Volant 会在 task 运行之前停止并点出这个方法，而不是通过你没有要求的机制提权。当运行需要提权，而方法来自 `--become-method`、`ansible.cfg` 或 `ansible_become_method` 时，运行会在任何 task 之前以退出码 2 结束。

`become_user` 可以是 `{{ app_user }}` 这样的模板。随循环项变化的会被拒绝，因为一个批次只能提权到一个用户。

`become_exe` 和 `become_flags` 从关键字以及 `ansible_become_exe` 和 `ansible_become_flags` 读取，主机变量优先。两者都按 shell 的方式拆分成词，引号不配对会让 task 失败。

### become 密码

密码来自以下之一：

- `-K`（`--ask-become-pass`），在 Unix 上会关闭终端回显。
- `--become-password-file`，读取文件的第一行。
- `ansible_become_password`、`ansible_become_pass`，或该方法自己的 `ansible_sudo_pass` 或 `ansible_su_pass`，后者优先于另外两个。哪个变量优先，见[主机变量](#主机变量)。

它只会写入 `sudo` 的标准输入一次。它绝不会出现在命令行、日志或结果中：携带它的类型在调试输出中打印 `<redacted>`，`sudo` 自己的错误输出在变成消息之前会被擦除其中的密码。

### sudo 探测

Volant 会询问 `sudo` 该用哪种形式，而不是假设其中一种。它先尝试 `sudo -n`，只有当 `sudo` 本身因缺少认证而拒绝请求时，才退回到 `sudo -k -S -p ''`。为什么靠猜测会让链路挂起，见[决策记录 0004](/zh-cn/docs/decisions/0004-become-runs-a-second-agent-under-sudo/)。对已经运行过的主机，Volant 先在做检查的同一个 `ssh` 会话里用 `sudo -n` 启动缓存的 agent，只有这样不行时才运行探测，见[主机上的 agent](/zh-cn/docs/internals/agent/)。

当提权失败时，无论是缺少密码、密码错误，还是 `sudoers` 规则禁止该命令，task 都会失败，主机仍然可达。消息是 Ansible 自己的：`Missing sudo password` 或 `Incorrect sudo password`。使用另一个密码的 task 会得到自己的 agent，因此即使正确的密码已经成功过，错误的密码仍会失败，而 `ignore_errors` 会像对待其他失败一样覆盖这种失败。

### su

`su` 没有在没有终端的情况下读取密码的标志，所以 Volant 改为回答它的提示。密码只会写入提示一次，绝不会放进命令行、日志或 `-vvv` 输出。密码错误会让 task 失败，并给出 `Incorrect su password`。提示始终不出现时，task 会以 `Timeout (12s) waiting for privilege escalation prompt` 结束，与 ansible-core 一致。

在 `local` 连接上，`su` 运行在它自己的终端上。密码错误会让 task 失败，并给出 `Premature end of stream waiting for become success.` 以及 `su` 打印的内容，与 ansible-core 在那里的做法一致。在本地连接上，等待提示的时间始终是 12 秒，不论 `-T` 设为多少。如果提示始终没有出现，主机会变为不可达，并给出 `Timed out waiting for become success or become password prompt.`

## 尚未支持

- Windows 主机。
- `ssh` 和 `local` 之外的连接插件，列在[支持矩阵](/zh-cn/docs/reference/parity/#collection-connection)中。
- `doas`、`pbrun` 以及其他 become 方法。它们可以作为启动 agent 的其他方式添加，无需更改协议。

完整列表见[关键字](/zh-cn/docs/reference/keywords/)，它由引擎读取的表格生成。
