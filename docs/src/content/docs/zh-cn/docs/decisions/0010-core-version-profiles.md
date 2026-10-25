---
sourceHash: 9e8e2288a96a
title: "按项目选择 ansible-core 版本，并保留 Ansible 移除的功能"
description: "决策记录 0010。"
sidebar:
  label: "0010 · 版本配置"
  order: 10
---

| 状态 | 日期 |
|---|---|
| 提议 | 2026-10-25 |

## 背景

Volant 复现 ansible-core 2.19.12（[决策 0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/)）。Ansible 一直在演进：2.20 和 2.21 已经发布，2.22 处于 release candidate 阶段。每个版本都会移除或改变 playbook 所依赖的行为。有些团队会留在今天使用的版本上，另一些团队想要最新的行为，单一的参考版本无法同时满足两者。

2026-10-25，golden 语料在 ansible-core 2.20.10、2.21.5 和 2.22.0rc1 下重新录制，去掉时间、job ID 和进程 ID 后，与 2.19.12 的录制结果比较。语料涵盖模板、清单，以及运行 1032 次的 355 个场景。2.20 下有 14 个文件不同，2.21 下有 76 个，2.22 下有 94 个。大多数变化很小，比如结果里多了一个键，或者错误前面多了一个前缀，但有些变化会改变一次运行：循环结果不再带 `skipped`，所以读取 `r.skipped` 的 playbook 在 2.21 下以退出码 2 停止。[附录](#附录2026-10-25-的语料差异)列出了这些差异。

## 决策

### 配置

配置（profile）是 Volant 复现其行为的 ansible-core 版本。目前有两个：`2.19` 是默认配置，保持 Volant 今天的行为；`2.22` 在 ansible-core 2.22.0 发布并录制其语料后加入。`latest` 指 Volant 自带的最新配置。

从 2.22 起，每个 ansible-core 版本都会新增一个配置，随附带其录制结果的 Volant 版本发布，`latest` 随之指向它。ansible-core 停止维护某个版本时，对应的配置也会移除。2.19 和 2.22 之间的版本没有自己的配置：它们的变化包含在 `2.22` 中。

项目通过 `ansible.cfg` 中的 `[volant] core_version` 或环境变量 `VOLANT_CORE_VERSION` 选择配置，环境变量优先于文件。取值是一个版本（`2.19`、`2.22`）或 `latest`。其他任何值都会以退出码 5 终止运行，并列出可用的配置。`volant.lock` 如何记录配置，与锁文件本身一起在 #653 中决定。

`ansible_version` 报告配置对应的版本：`2.19` 下是 2.19.12，`2.22` 下是录制 `2.22` 语料时使用的补丁版本。这扩展了[决策 0003](/zh-cn/docs/decisions/0003-ansible-version-reports-the-reference-release/)。`volant_version` 不变。

Python 模块来自控制节点上安装的 ansible-core（[决策 0006](/zh-cn/docs/decisions/0006-warm-python-path/)），因此无论配置如何，它们的结果都跟随该版本。安装的版本与配置不一致时，Volant 会在运行开始时发出警告。

### 按行为设置开关

只有一个引擎。版本之间的每个差异都是一个有名字的开关，并标明它从哪个版本开始，例如从 2.21 开始的 `loop-results-omit-skipped`，代码向配置询问该开关是否开启。每个开关在下面的注册表中都有一个同名条目。

分开的行为集合被放弃了：测得的差异很小，而且散布在整个引擎中，每个版本复制一份会为了这里改一个键、那里改一条消息而重复几乎全部代码。

模块和 action plugin 的代码从不分叉。原生模块或 action plugin 只有一个实现，版本之间的差异是其内部的一个开关。开关由一个场景测试，该场景在它所改变的每个版本下都有录制，golden 测试在每个配置下运行每个场景，并与该配置的录制结果比较。

### 每个配置的语料

`just golden` 接受版本作为参数，用安装在独立 `uv` 环境中的该版本 ansible-core 录制。`2.19` 的录制结果保持原位。其他配置在 `expected.json` 旁边写入 `expected.<版本>.json`，只针对结果不同的场景：2.22 下是 355 个中的 94 个。

控制节点的 Python 与版本一起固定。测量发现它也会改变结果：对同样的 2.19 模板，Python 3.12 和 3.13 对 `'abcabc'.replace('b', 'X', count=1)`、对传给字符串方法的错误参数，以及对包含 `nan` 的列表排序，给出的结果不同。

在 4 核机器上，录制模板、清单和场景的 golden 每个版本大约需要 52 分钟。由维护者录制一次，和现在一样，CI 从不录制。CI 把 Volant 与仓库中的录制结果比较，每个配置多运行一次 golden 测试。

只有带着该版本的录制结果，一项行为才能进入配置。

### 注册表

注册表列出 Ansible 移除或改变的内容，位于 `crates/volant/src/profile/registry.toml`。每个条目包含 `name`（开关名）、功能（`feature`）、移除或改变它的版本（`removed_in`）、类型（`kind`，`security` 或 `compat`）、替代方案（`replacement`）、警告（`warning`），以及针对已知上游 bug 的 Volant 所遵循的行为（`follow`，`release` 或 `documented`）。

```toml
[[change]]
name = "debug-shows-registered-secret"
feature = "showing a registered secret with `debug`"
removed_in = "2.22"
kind = "security"
replacement = "write the value to a file with `copy` and `no_log: true`"
warning = "showing a registered secret with debug was removed in ansible-core 2.22"

[[change]]
name = "interpreter-auto-legacy"
feature = "`interpreter_python = auto_legacy` and `auto_legacy_silent`"
removed_in = "2.21"
kind = "compat"
replacement = "`auto` or `auto_silent`"
warning = "interpreter_python = auto_legacy was removed in ansible-core 2.21"

[[change]]
name = "loop-results-omit-skipped"
feature = "`skipped: false` in each loop result"
removed_in = "2.21"
kind = "compat"
replacement = "`r.skipped | default(false)`"
warning = "loop results lost their skipped key in ansible-core 2.21"
```

在等于或晚于 `removed_in` 的配置下，`compat` 功能继续可用，并给出警告，后面跟着 “Volant still runs it” 和替代方案。`security` 功能保持关闭，除非设置 `[volant] allow_insecure_legacy = true`。在更早的配置下，该功能照常存在，没有警告。

当版本说明把变化与某个 CVE 联系在一起，当被移除的行为可能显示秘密，或者当它让 playbook 内容读写超出给定范围的东西时，条目属于 `security`。ansible-core 在 2.19.12 之后的 2.19 补丁版本中发布的安全修复，同样适用于 `2.19` 配置。已知的 `security` 条目有：

- `debug` 显示已注册的秘密，以及结果中的 `no_log` 选项被占位文本替换，两者都在 2.22 中改变；
- 在所有输出和 `log_path` 中用 `$REDACTED$` 遮盖秘密，2.22；
- `tempfile` 在 `prefix` 和 `suffix` 中接受路径，在 2.19.13、2.20.9 和 2.21.4 中移除；
- `uri`、`get_url`、`apt_key`、`rpm_key` 和 `url` lookup 在 URL 中显示的凭据，从 2.19.13、2.20.9 和 2.21.4 起被遮盖；
- ansible-connection 的持久化目录没有私有权限，在 2.19.14、2.20.10 和 2.21.5 中修复；
- `ansible-galaxy collection install` 接受指向集合之外的 `FILES.json` 条目（CVE-2026-89091），2.22.0rc1。

迁移指南中的其他移除都属于 `compat`，其中包括 `DEFAULT_TRANSPORT=smart` 和 `vault` 过滤器的 `vaultid` 参数（2.20）、`paramiko` connection plugin 和 `git` 的 `gpg_whitelist` 别名（2.21），以及 `follow_redirects` 的 `yes` 和 `no`（2.22）。

### `volant upgrade-plan`

```sh
volant upgrade-plan [PATH...] [--to PROFILE] [--json]
```

该命令读取 `PATH`（默认为当前目录）下的 playbook、角色、清单和 `ansible.cfg`，列出目标配置（默认 `latest`）相对于当前配置所移除或改变的每一处功能用法。每条结果给出 `文件:行`、注册表条目和建议的修改。

有些变化无法通过阅读发现，例如通过计算出的名字读取的结果，或者错误的文本。报告最后列出两个配置之间它无法检查的注册表条目。对于这些条目，项目可以在当前版本下录制一次运行，再用 [`volant shadow replay`](/zh-cn/docs/start/switch-without-risk/) 在目标配置下回放。出现的分歧就是这次运行会遇到的变化。

退出码：什么都没发现时为 0，至少发现一处用法时为 1，无法读取文件时为 4，配置未知时为 5。`--json` 为每条结果输出一个对象：`path`、`line`、`name`、`removed_in`、`kind` 和 `suggestion`。

### 已知回归

变化是有意为之时，Volant 跟随该版本。当版本的行为是上游尚未修复、会在主机上执行错误操作的 bug 时，Volant 跟随文档描述的行为。答案逐案决定，写入注册表的 `follow` 字段。对于 2.21 的已知回归：

- 解释器发现复用了另一台主机的解释器路径（[ansible/ansible#87288](https://github.com/ansible/ansible/issues/87288)）：`documented`，每台主机各自进行发现；
- 发现时的警告给出的是清单主机而不是被委派的主机（[ansible/ansible#87298](https://github.com/ansible/ansible/issues/87298)）：`documented`；
- 被委派的主机没有缓存发现结果（[ansible/ansible#86517](https://github.com/ansible/ansible/issues/86517)）：`documented`，因为只有耗时不同；
- dnf 插件向 stdout 输出时 `dnf` 失败（[ansible/ansible#87366](https://github.com/ansible/ansible/issues/87366)）：`release`。该模块从控制节点的 ansible-core 运行，Volant 不修补模块。在 changelog 和 issue 跟踪中都没有发现成功的 `dnf` 输出有任何变化。

### 锁文件

`volant.lock` 固定运行项目的引擎。它是否记录配置，以及锁文件与 `core_version` 不一致时运行如何处理，在 #653 中决定。

## 影响

- 什么都不设置的项目保持 `2.19` 的行为。
- 切换到 `latest` 的项目保留被移除的功能并收到警告，安全相关的除外。
- 每个差异在进入配置之前，都需要一个开关、一个注册表条目和一份录制结果。
- 实现将在以下 issue 中进行，按规模列出：
  - 配置设置、`VOLANT_CORE_VERSION`、按配置的 `ansible_version`，以及对控制节点 ansible-core 的检查（中）；
  - 按配置录制：`just golden` 的版本参数、`expected.<版本>.json` 和按配置的 golden 测试（中）；
  - 注册表、其警告和 `allow_insecure_legacy`（中）；
  - `2.22` 配置的各个开关，依据下面的附录（大，设置完成后按领域拆分）；
  - `volant upgrade-plan`（中）。

## 附录：2026-10-25 的语料差异

ansible-core 2.20.10、2.21.5 和 2.22.0rc1 与 2.19.12 对比。前三个在 Python 3.12 下运行，2.22.0rc1 在 Python 3.13 下运行。未列入：`order: shuffle` 和 `throttle` 下的主机顺序，每次运行都不同；job 文件路径和进程 ID；`ansible_version` 本身；以及来自 Python 而非 ansible-core 的模板差异。

| 功能 | 2.19.12 | 2.20 | 2.21 | 2.22 | 类型 |
|---|---|---|---|---|---|
| 使用未知 `match_type` 的 `regex` 测试 | `'str' object is not callable` | `Invalid match_type specified` | 同 2.20 | 同 2.20 | 模板 |
| 指定盐值的 `password_hash` | 使用该盐值（`$1$saltsalt$…`） | 同 2.19 | 另一个盐值（`$1$n34PoBLM$…`） | 同 2.21 | 模板 |
| `when` 中的错误 | `Error while evaluating conditional: …` | 同 2.19 | `A 'when' expression failed: …` | 同 2.21 | 模板 |
| 失败命令的 `msg` | `non-zero return code` | 同 2.19 | `The command exited with a non-zero return code.` | 同 2.21 | 模块 |
| `include_vars` 的错误 | 路径不带引号 | 路径带引号 | 同 2.20 | 同 2.20 | 模块 |
| `copy` 在 check mode 下的结果 | 没有 `file` 键 | 有 `file` 键 | 同 2.20 | 同 2.20 | 模块 |
| `chdir` 失败的 `command` | `rc: null` | 同 2.19 | 没有 `rc` 键 | 同 2.21 | 模块 |
| 找不到的命令的 `cmd` | 字符串 | 同 2.19 | 同 2.19 | 列表 | 模块 |
| 模块结果中的 `ansible_parsed` | 无 | 无 | 无 | `true` | 模块 |
| 循环结果中的 `skipped: false` | 有 | 有 | 无 | 无 | 场景 |
| `changed_when_result` | 无 | 无 | 有 | 有 | 场景 |
| `failed_when` 的消息 | `Action failed: Unknown error.` 或动作名 | 同 2.19 | `A 'failed_when' expression evaluated to 'True'.` | 同 2.21 | 场景 |
| `until` 的重试行和 `attempts` | `(1 retries left)`、`attempts: 1` | 同 2.19 | `(0 retries left)`、`attempts: 2` | 同 2.21 | 场景 |
| 读取同一循环中设置的 fact 的循环项 | `a-1`、`b-2`、`c-3` | 同 2.19 | `a-0`、`b-1`、`c-2` | 同 2.21 | 场景 |
| 失败后的 `any_errors_fatal: "{{ false }}"` | 另一台主机停止 | 同 2.19 | 另一台主机继续 | 同 2.21 | 场景 |
| `include_tasks` 中缺失的文件 | `include` 和 `reason` 键 | 同 2.19 | `msg` 和一行 `[ERROR]` | 同 2.21 | 场景 |
| 缺失的 `copy` 源文件 | 单行消息 | 同 2.19 | `<<< caused by >>>` 和搜索过的路径 | 同 2.21 | 场景 |
| 角色之外的角色文件 | 退出码 4 | 退出码 4 | 退出码 1 | 退出码 1 | 场景 |
| `debug` 未定义的 `msg` | `{"msg": null}` | 同 2.19 | `{}` | 同 2.21 | 场景 |
| 带角色参数规范的 `--start-at-task` | 跳过校验 | 执行校验 | 同 2.20 | 同 2.20 | 场景 |
| `ansible --help` | 只有 `-K` | `-K \| --become-password-file` | 同 2.20 | 同 2.20 | 场景 |
| 输出中的秘密值 | 显示 | 显示 | 显示 | `$REDACTED$`，路径中也是 | 场景 |
| `ansible-inventory --graph` 中 vault 加密的值 | 显示 | 显示 | 显示 | `$REDACTED$` | 清单 |

在 2.22 下，遮盖还隐藏了输出中并非秘密的部分：become 密码 `wrong` 把目录 `become-su-local-wrong-password` 变成了 `become-su-local-$REDACTED$-password`。
