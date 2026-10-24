---
sourceHash: 760ddc00f1a2
title: 决策记录
description: 塑造 Volant 用户所见内容的决策，以及做出这些决策的原因。
sidebar:
  label: 关于决策记录
  order: 0
---

一份决策记录解释一个影响 Volant 用户的选择：当时的情形是什么，决定了什么，以及由此带来什么后果。这些记录采用 [MADR](https://adr.github.io/madr/) 格式，按做出决策的先后编号。

一份记录在被接受之后不会被重写。后来的决策改变它时，状态行会说明这一点，并链接到新的记录。决策之后进行的测量，会作为带日期的小节追加在末尾。

| 记录 | 决策 |
|---|---|
| [0001](/zh-cn/docs/decisions/0001-gpl-3-0-or-later-license/) | 仓库采用 GPL-3.0-or-later 许可。接口 crate 是例外，见 0009。 |
| [0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/) | 复现 ansible-core 2.19，任何差异都视为 bug。 |
| [0003](/zh-cn/docs/decisions/0003-ansible-version-reports-the-reference-release/) | `ansible_version` 报告参照版本；`volant_version` 报告 Volant 自己的版本。 |
| [0004](/zh-cn/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) | `become` 在 `sudo` 下启动第二个 agent。 |
| [0005](/zh-cn/docs/decisions/0005-fork-permits-span-host-local-batches/) | 在 `batching` 选项之下，主机在主机本地步骤之间保留它的 fork 许可。 |
| [0006](/zh-cn/docs/decisions/0006-warm-python-path/) | 每次运行发送一个 Python 模块的 union blob，并从预热的服务 fork 它们。 |
| [0007](/zh-cn/docs/decisions/0007-action-plugins/) | 把 action plugin 作为由控制节点驱动的一串子 task 来运行。 |
| [0008](/zh-cn/docs/decisions/0008-native-modules-and-speed/) | 常用模块在 agent 中原生回答，其余交给 Python，每台主机共享一个 ssh 连接。 |
| [0009](/zh-cn/docs/decisions/0009-interface-crates-mit-or-apache/) | 引擎保持 GPL-3.0-or-later，描述公共接口的 crate 采用 MIT OR Apache-2.0 许可。 |
