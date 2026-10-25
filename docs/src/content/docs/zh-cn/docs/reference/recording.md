---
sourceHash: 121b3af3c375
title: 运行记录
description: ansible-playbook 运行时 volant.compat.record 回调写入的文件，逐行说明。
---

团队在切换之前，可以记录 `ansible-playbook` 在真实运行中做了什么，之后再让 Volant 重放这些运行。记录由 `volant.compat.record` 回调写入。它随 `volant.compat` collection 一起发布，只需要 Python 标准库，可在 ansible-core 2.19 及更高版本上运行。

## 启用

把 collection 安装到 ansible-core 查找 collection 的位置，然后在 `ansible.cfg` 中加入两个小节：

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

它是一个通知类回调。它从不替换 stdout 回调，运行的输出和退出码与不启用它时相同。

| 设置 | 含义 |
|---|---|
| `path` | 要写入的文件。`{{ timestamp }}` 会替换为运行开始的 UTC 时间，格式为 `20261025T093000Z`；不读取其他模板。以 `.gz` 结尾的文件名会用 gzip 压缩。缺失的父目录以 `0700` 模式创建。默认值为 `shadow/run-{{ timestamp }}.jsonl.gz`。 |

`VOLANT_RECORD_PATH` 可以通过环境变量设置 `path`，适合 CI 任务。

文件无法写入时，回调会打印一条警告，运行继续进行，只是没有记录。

## 安全

记录保存的是 ansible-core 看到的任务结果。其中可能包含解密后的 vault 值、本次运行的 extra 变量，以及模块返回的全部内容。请把它当作机密：

- 文件以 `0600` 模式创建，已存在的文件会被重新设为 `0600`。
- 不要把记录目录纳入版本控制；CI 中的记录应作为私有制品保存，并设置较短的保留期。
- 带 `no_log` 的任务保留其状态，结果记录为 `{"censored": true}`。

## 格式

记录采用 JSON Lines：每行一个 JSON 对象，每个对象都有一个 `event` 键。本页描述格式 `1`。

| `event` | 字段 |
|---|---|
| `header` | `format`、`ansible_core`（其版本）、`cli`（ansible-core 解析出的本次运行的命令行选项）、`cwd`、`playbooks`（每个 playbook 的 `path` 和 `sha256`）、`inventory`（原样给出的 `sources`，以及每个文件类来源的 `path` 和 `sha256`）、`extra_vars_hash`、`started` |
| `inventory` | `hosts`：清单中的每台主机，以及它除 `all` 之外所属的组 |
| `play` | `uuid`、`name`、`hosts`（play 的主机模式） |
| `task` | `uuid`、`path`（`文件:行号`）、`name`、`action`（模块或动作的完整名称）、`handler`、`role` |
| `result` | `host`、`task`（其 `uuid`）、`item`、`status`、`delegated_to`、`result` |
| `retry` | `host`、`task`、`attempt`、`result`：`until` 循环中一次失败的尝试 |
| `include` | `file`、`hosts`：`include_tasks` 或 `include_role` 引入的文件，以及运行它的主机 |
| `notify` | `handler`、`host`：将在某台主机上运行的 handler |
| `stats` | `hosts`（每台主机的汇总）、`custom`（`set_stats` 收集的内容） |

`header` 和 `inventory` 两行最先写入，在第一个 play 开始时。`stats` 是完整运行的最后一行，因此缺少它的记录来自一次被中断的运行。

字段说明：

- 所有哈希均为十六进制的 SHA-256。`extra_vars_hash` 基于按键排序的 JSON 形式的 extra 变量计算。
- `status` 取值为 `ok`、`changed`、`failed`、`skipped` 或 `unreachable`。被 `ignore_errors` 放过的失败仍记为 `failed`。
- 在循环中，每个元素都有一行 `result`，其 `item` 为从 0 开始的索引。随后 `item` 为 `null` 的那一行是整个任务，各元素位于 `results` 下。
- `delegated_to` 是 `delegate_to` 指定的主机，没有则为 `null`。
- `result` 是回调收到的任务结果，包括 ansible-core 添加的键，例如 `invocation`、`false_condition`、`skip_reason` 和 `attempts`。这些键是否存在取决于版本：ansible-core 2.21 不再把 `invocation` 传给回调。
- 收集 facts 的结果会完整记录，因为重放时要用它渲染模板。每台主机约 50 到 100 KB，所以默认使用 gzip。
- `include_tasks` 自身会产生一行 `task` 和每台主机一个 `result`，然后每个文件一行 `include`。`import_tasks` 两者都不产生，只产生被导入的任务。
- `notify` 行在 handler 运行时写入，位于通知它们的任务之后。
