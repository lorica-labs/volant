---
sourceHash: 06ef07eefbf0
title: Vault
description: Volant 如何读取 Ansible Vault 数据，从哪里获取密码，以及为什么密码永远不会离开控制节点进程。
---

Volant 读取和写入 ansible-core 2.19.12 的 vault 1.1 和 1.2 格式。结果、错误消息和退出码都与 ansible-core 一致。

## 密码从哪里来

| 来源 | 说明 |
|---|---|
| `-J`、`--ask-vault-pass` | 在终端中询问，不回显。 |
| `--vault-password-file`、`--vault-pass-file` | 一个文件，或一个会打印密码的可执行文件。可重复使用。 |
| `--vault-id LABEL@SOURCE` | 来源是文件、可执行文件或 `prompt`。可重复使用。 |
| `[defaults] vault_password_file`、`vault_identity_list` | 同上，来自 `ansible.cfg`。 |
| `ANSIBLE_VAULT_PASSWORD_FILE`、`ANSIBLE_VAULT_IDENTITY_LIST` | 同上，来自环境变量。 |

每个密码都会对每个 vault 加密的值逐一尝试，不论其标签是什么。

## 会解密什么

- playbook 或变量文件中的内联 `!vault` 值。它们作为密文加载，在模板读取时解密。解密后的值保留 ansible-core 赋予它的信任级别。
- 整个 vault 加密的文件：`vars_files`、`include_vars`、`group_vars` 和 `host_vars` 文件。无法读取的 vault 文件会失败，消息与 ansible-core 一致。
- `vault` 和 `unvault` 过滤器、`vault_encrypted` 和 `vaulted_file` 测试器，以及 `unvault` lookup。
- `copy`、`template` 和 `script` 的 vault 加密 `src`，以及 `unarchive` 的 vault 加密归档。运行会在插件读取之前，把它解密到一个私有的临时目录中。`copy` 或 `unarchive` 上的 `decrypt: false` 会保留密文。

任何机密都打不开的 `!vault` 值，会让读取它的 task 失败，并附上 ansible-core 打印的三个块。`debug: var=x` 以退出码 2 结束，原因指向写下该值的那一行。`is vault_encrypted` 针对当前主机的变量应答。

## 机密留在控制节点

vault 密码以内存形式存在于控制节点进程中。它永远不会被写入命令行、子进程的环境变量、日志、结果或 agent 链路。连接密码和提权密码遵循同样的规则，SSH 密码如何到达 `ssh`，见[连接](/zh-cn/docs/hosts/connections/#连接类型)。

在 Python 中运行的过滤器、测试器和 lookup 永远看不到 vault 密码。vault 相关的那些正是出于这个原因在控制节点进程中运行。见[需要 Python 的过滤器、测试器和 lookup](/zh-cn/docs/variables/templating/#需要-python-的过滤器测试器和-lookup)。

从受管主机返回的值永远不会用这次运行的机密解密，也永远不会再次作为模板渲染。见[来自主机的值绝不会被当作模板](/zh-cn/docs/variables/templating/#来自主机的值绝不会被当作模板)。

## 已知缺口

- `vault` 过滤器的 `salt` 和 `wrap_object` 参数会被拒绝，提示尚不支持。
- 写在 play 自己的 `vars:` 里的 `!vault` 值，在没有机密能打开它时，不会打印 `Origin:` 行。
- 对持有 vault 加密值的裸 `{{ x }}` 做 `set_fact`，会丢失 vault 标记，所以 `is vault_encrypted` 在该 fact 上回答 false，而 ansible-core 回答 true。

[命令行页面](/zh-cn/docs/reference/cli/#vault)列出了这些选项。
