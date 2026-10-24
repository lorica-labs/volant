---
sourceHash: 85339820dcff
title: 运行失败时
description: 找出失败的 task 和原因，用 ansible-playbook 运行同一个 playbook 做对比，并报告你的发现。
---

先看退出码。它会告诉你，运行是在接触主机之前就停止了，还是在某台主机上失败了。

| 退出码 | 发生了什么 | 去哪里看 |
|---|---|---|
| 4，没有汇总 | playbook 在第一次连接之前就停止了。它无法解析，或者用到了这个版本尚不支持的内容。 | 退出码上方的消息。见[运行前就停止](#运行前就停止)。 |
| 2、4 或 6，有汇总 | 有主机失败（2）、有主机不可达（4），或两者都有（6）。 | task 的 `fatal:` 行，然后看[运行中失败](#运行中失败)。 |
| 1 | 找不到 role、playbook 或主机模式。 | 消息中引用的文件或名称。 |

[退出码](/zh-cn/docs/reference/exit-codes/)列出了所有退出码。

## 运行前就停止

这个版本无法运行的内容，大部分会在 Volant 连接任何主机之前就被指出。消息会说明它拒绝了什么，以及在哪里：

| 消息 | 怎么办 |
|---|---|
| `task 'NAME': keyword 'KEYWORD' is not supported yet on 'MODULE'` | 在[关键字](/zh-cn/docs/reference/keywords/)中查看这个关键字。删掉它，或者暂时让这个 play 继续使用 `ansible-playbook`。 |
| `task 'NAME': argument 'ARGUMENT' is not supported yet on 'MODULE'` | 在[模块](/zh-cn/docs/reference/modules/)中查看这个模块。大部分被拒绝的参数都能在那里找到变通办法。 |
| `couldn't resolve module/action 'NAME'` | 检查拼写。如果模块来自某个 collection，用 `volant runtime install --collections NAMESPACE.COLLECTION` 安装它。 |
| `strategy 'NAME' is not supported yet` | 使用 `linear`、`free`、`host_pinned` 或 `debug`。 |
| `become_method 'NAME' is not supported yet` | 使用 `sudo` 或 `su`。这种情况以退出码 2 结束。 |
| `the role 'NAME' was not found in ...` | 消息会列出它搜索过的目录。把 role 所在的目录加入 `roles_path`。这种情况以退出码 1 结束。 |

[我的 playbook 能运行吗？](/zh-cn/docs/start/compatibility/#目前会拒绝什么)列出了这些拒绝情形，并介绍了如何不运行就检查整个项目。[预检](/zh-cn/docs/internals/preflight/)说明了这项检查的工作方式。

## 运行中失败

1. 只在失败的主机上重新运行，并输出更多信息。`-vvv` 会打印每个模块收到的参数：

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 -vvv
   ```

2. 跳过已经通过的 task。`--start-at-task` 从你指定的 task 开始，`--step` 会在每个 task 之前询问：

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 --start-at-task "Install nginx"
   ```

3. 在失败处停下来，看一看周围的情况。在 play 或 task 上设置 `debugger: on_failed` 后，task 失败时 Volant 会打开 Ansible 的 task 调试器。你可以打印变量、修改参数，再重新运行这个 task。

## 与 ansible-playbook 对比

Volant 读取相同的文件，所以同样的命令也可以在 Ansible 下运行：

```sh
ansible-playbook -i inventory.ini site.yml --limit web1
```

如果 Ansible 以同样的方式失败，问题出在 playbook 或主机上。如果只有 Volant 失败，或者两者结果不同，那就是 Volant 的 bug。在修复之前，你可以让这个 play 继续使用 `ansible-playbook`。

## 回到 ansible-playbook

同样的文件可以原样在 `ansible-playbook` 下运行。Volant 不会给你的 playbook、role、inventory 或 `ansible.cfg` 添加任何东西，所以没有什么需要转换回去。

在主机上，留下的只有缓存的 agent：`~/.ansible/tmp` 里名为 `volant-agent-<version>-<uid>` 的目录，如果运行用到过 Python 模块，旁边还有一个 `volant-blobs-<version>-<uid>` 目录。要删除它们，请在每台主机上以运行所用的账户删除：

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-*'
```

如果主机上有 task 使用过 `become`，在它所提权到的账户的主目录里还有第二份。请以那个账户运行同样的命令。要把 Volant 从控制节点上也移除，见[卸载](/zh-cn/docs/start/installation/#卸载)。[Volant 会在你的机器上留下什么](/zh-cn/docs/#volant-会在你的机器上留下什么)列出了全部内容。

## 报告问题

- 与 `ansible-playbook` 的差异，请提交 [Ansible 兼容性缺口](https://github.com/lorica-labs/volant/issues/new?template=compat.yml)报告。
- 其他与文档不符的问题，请提交 [bug 报告](https://github.com/lorica-labs/volant/issues/new?template=bug.yml)。
- 安全问题请按[安全策略](https://github.com/lorica-labs/volant/blob/main/SECURITY.md)处理，绝不要发在公开的 issue 里。

请附上 `volant --version` 的输出、仍会失败的最小 playbook 和 inventory，以及用 `-vvv` 运行失败时的输出。发布之前，请删除密码、密钥、vault 内容和主机名。公开的 issue 会一直公开。
