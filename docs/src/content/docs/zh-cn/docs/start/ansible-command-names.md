---
sourceHash: 8dc270cc846e
title: ansible 命令名
description: Volant 的哪个命令取代 ansible-core 的哪个程序，哪些相同，哪些不同，以及如何保留你的命令行。
---

Volant 运行 ansible-core 2.19.12 的九个程序。每一个都是 `volant` 的子命令，在 Linux 和 macOS 上也可以用它的 ansible 名字启动。无论是程序名还是链接，都不支持 Windows。

## 什么取代什么

| ansible-core 程序 | Volant 命令 | 运行于 | 托管运行时 |
|---|---|---|---|
| `ansible` | [`volant adhoc`](/zh-cn/docs/reference/cli/adhoc/) | Volant | 原生模块不需要 |
| `ansible-playbook` | [`volant playbook`](/zh-cn/docs/reference/cli/)、`volant-playbook` | Volant | 在第一次需要 Python 的使用时安装 |
| `ansible-vault` | [`volant vault`](/zh-cn/docs/reference/cli/vault/) | Volant | 不需要 |
| `ansible-inventory` | [`volant inventory`](/zh-cn/docs/reference/cli/inventory/) | Volant | 不需要，collection 的 inventory 插件除外 |
| `ansible-doc` | [`volant doc`](/zh-cn/docs/reference/cli/doc/) | ansible-core 自己的工具 | 第一次使用时安装 |
| `ansible-config` | [`volant config`](/zh-cn/docs/reference/cli/config/) | ansible-core 自己的工具 | 第一次使用时安装 |
| `ansible-galaxy` | [`volant galaxy`](/zh-cn/docs/reference/cli/galaxy/) | Volant，正在编写：子命令在成为原生实现之前会被拒绝 | 不需要 |
| `ansible-pull` | [`volant pull`](/zh-cn/docs/reference/cli/pull/) | Volant | `git` 检出不需要 |
| `ansible-console` | [`volant console`](/zh-cn/docs/reference/cli/console/) | Volant | 原生模块不需要 |

托管运行时在某个命令第一次需要它时自行安装，与 playbook 一样，并在安装时于标准错误输出打印一行。[托管运行时](/zh-cn/docs/reference/runtime/)对它有说明。你不需要安装 ansible，但九个命令中有两个运行的是该运行时里 ansible-core 自己的代码。

## 保留你的命令行

除非你要求，否则不会创建链接。要么用 `--shims` 运行安装脚本，要么运行 [`volant shims install`](/zh-cn/docs/reference/cli/shims/)：

```sh
curl -fsSL https://volant.sh/install.sh | sh -s -- --shims
volant shims install
```

每个名字都是指向 `volant` 的符号链接。Volant 会读取自己被启动时的文件名，所以链接 `ansible-vault` 运行的是 `volant vault`。两个命令接受相同的参数，给出相同的输出和退出码。用法错误以 2 退出，并按启动时的名字称呼程序，即 `ansible-vault: error: ...`。

`install` 不会替换不是它创建的文件或链接，除非给出 `--force`。当 `PATH` 中还有同名的其他程序时，它会说明：链接胜出时是 `note: /usr/bin/ansible-vault is hidden by this link, DIR comes first in PATH`，否则是 `note: ... still runs first, DIR comes later in PATH`。要删除这些链接：

```sh
volant shims remove
```

`remove` 只删除指向名为 `volant` 的文件的链接，其他一切都保持原样。

像下面这样的 CI 脚本，在链接存在后原样即可运行，机器上不需要 ansible：

```sh
ansible-inventory -i hosts.ini --list > inventory.json
ansible-vault view group_vars/all/secrets.yml --vault-password-file ci-pass
ansible-playbook -i hosts.ini site.yml
```

## 哪些相同

对于 `ansible`、`ansible-vault` 和 `ansible-inventory`，输出、各命令页面上列出的选项以及退出码都已与 ansible-core 2.19.12 对照测量。每个说法背后都有测试。

| 说法 | 在哪里检查 |
|---|---|
| `volant adhoc` 打印 `minimal` callback 的字节、`[ERROR]` 块以及 `-o` 和 `-t` 的形式，退出码与参考实现一致 | `adhoc-*` 场景 golden 和 `crates/volant/tests/adhoc_cli.rs` |
| 名字 `ansible` 运行 `volant adhoc` | `the_ansible_name_runs_adhoc` |
| 名字 `ansible-playbook` 运行 `volant playbook` | `a_link_named_ansible_playbook_behaves_as_the_playbook_subcommand` |
| `volant vault` 给出 ansible-vault 的消息、退出码、文件权限和文件 | `crates/volant/tests/vault_cli.rs`，以及 playbook 读取的 vault 数据所对应的 `vault-*` 场景 golden |
| 一个工具加密的文件，另一个工具能解密，默认 id 和带标签的 id 都一样 | `a_file_one_tool_encrypts_the_other_decrypts` |
| `volant inventory` 打印 ansible-inventory 的 JSON 和 YAML 字节：键顺序、四个空格的缩进、PyYAML 锚点 | `crates/volant/tests/inventory_golden.rs`（基于 `golden/inventories/cases.yml`）和 `inventory_cli.rs` |
| `volant config` 打印工具所打印的内容 | `crates/volant/tests/tools_cli.rs` |
| `volant galaxy` 接受 ansible-galaxy 的选项，并以相同的消息完成它最先做的检查 | `crates/volant/src/galaxy/tests.rs`，以及针对假 Galaxy 服务器的 `galaxy_golden.rs` |
| `volant pull` 打印的 playbook 运行和 `-o` 那一行与 `ansible-pull` 相同，检出时不用 Python | `console_pull.rs` 中的 `a_pull_prints_what_the_reference_pull_prints` 和 `a_pull_checks_out_and_runs_without_python` |
| 在管道上，`volant console` 为它的命令、模块和 shell 行打印的内容与 `ansible-console` 相同，原生模块不需要 Python | `console_pull.rs` 中的 `the_console_reads_its_commands_from_a_pipe` 和 `the_console_runs_without_python` |
| `install` 创建的链接、它打印的行，以及 `remove` 不动的内容 | `crates/volant/tests/shims.rs` |
| 哪些名字对应哪个命令，且没有其他名字 | `crates/volant/src/cli/program.rs` 中的测试 |

每个原生命令接受的选项列在它自己的页面上：[`volant adhoc`](/zh-cn/docs/reference/cli/adhoc/)、[`volant vault`](/zh-cn/docs/reference/cli/vault/)、[`volant inventory`](/zh-cn/docs/reference/cli/inventory/) 和 [`volant pull`](/zh-cn/docs/reference/cli/pull/)。没有列在那里的选项会被拒绝，绝不会被忽略。

## 哪些不同

- 帮助文本是 Volant 自己的，从不与参考实现比较。
- `--version` 打印 `ansible [core 2.19.12]`（或程序名）和各路径。当 Volant 找不到带 ansible-core 的解释器时，Python、Jinja 和 PyYAML 的行会省略，因为 `--version` 不安装任何东西。见 [`volant adhoc`](/zh-cn/docs/reference/cli/adhoc/)。
- `--version` 中的 `executable location` 是程序被启动时的样子：`volant`，或那个链接。见 [`volant doc`](/zh-cn/docs/reference/cli/doc/#终端退出码和---version)。
- `ansible-inventory --toml` 不需要任何 Python 包（参考实现需要 `tomli-w`），遇到 `null` 值时只输出一行 `[ERROR]`，而不是回溯信息（两者都以 250 退出）。见 [`volant inventory`](/zh-cn/docs/reference/cli/inventory/)。
- `volant pull` 先打印它的前两行，再打印各步骤的输出，它的检出也不启动 Python。见 [`volant pull`](/zh-cn/docs/reference/cli/pull/#与-ansible-pull-的不同)。
- 运行时有了 `collections/` 目录之后，`volant config dump` 会把 `COLLECTIONS_PATHS` 显示为由环境变量设置。见 [`volant config`](/zh-cn/docs/reference/cli/config/#collection-路径)。
- `ansible-vault view` 只在终端上才经过分页器，依次看 `PAGER`、`ANSIBLE_PAGER`、`[defaults] pager`，最后是 `less`。见 [Vault](/zh-cn/docs/variables/vault/)。
- `volant adhoc` 和 `volant playbook` 使用 Volant 的引擎，所以 [我的 playbook 能运行吗？](/zh-cn/docs/start/compatibility/) 和 [`volant adhoc`](/zh-cn/docs/reference/cli/adhoc/#不是模块的动作) 中列出的差异都适用。

## 退出码

| 命令 | 0 | 1 | 2 | 4 | 5 | 250 |
|---|---|---|---|---|---|---|
| `volant adhoc` | 每台主机都成功或被跳过 | 没有可作为目标的主机，或 `meta` 动作错误 | 有主机失败，或用法错误 | 有主机无法到达，或这个版本尚不支持的内容 | 设置无效，或 `command` 没有参数 | 参数不是有效的 JSON |
| `volant vault` | 完成 | 文件错误，例如对已存在的文件执行 `create` | 用法错误 | | 选项错误，例如 `--output` 带两个文件 | |
| `volant inventory` | 已打印 | | 用法错误 | | 没有动作或有两个动作，或 `--host` 或 `--graph` 的目标错误 | |

`volant playbook` 使用[退出码](/zh-cn/docs/reference/exit-codes/)页面上的退出码。在 ansible-core 的运行时中运行的三个工具返回工具自己的退出码；无法启动解释器时返回 1，并带一行 `[ERROR]:`。检出失败时 `volant pull` 以 2 退出，选项错误或找不到 playbook 时以 5 退出，其他情况下返回 playbook 的退出码。
