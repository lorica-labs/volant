---
sourceHash: 8cfc8ab189fd
title: "整个仓库使用 GPL-3.0-or-later"
description: "决策记录 0001。"
sidebar:
  label: "0001 · 许可证"
  order: 1
---

| 状态 | 日期 |
|---|---|
| 已接受；范围被 [ADR 0009](/zh-cn/docs/decisions/0009-interface-crates-mit-or-apache/) 缩小，该决策把描述公共接口的 crate 改为 MIT OR Apache-2.0 许可 | 2026-09-05 |

## 背景

Volant 的目标是执行未经修改的 Ansible 模块。为此，控制节点将通过嵌入的 Python 解释器调用 ansible-core 自己的 payload 构建器，agent 将把 ansible-core 的 `module_utils` 传送到受管主机。ansible-core 采用 GPL-3.0 许可。在运行时导入一个 GPL 库并重新分发其中的部分内容，会让 Volant 采用宽松许可证在法律上站不稳。

这个版本还没有做到那一步。它没有嵌入 Python 解释器，不含任何 ansible-core 代码，只运行原生模块。选择这个许可证，是为了项目正在走向的架构，而不是它现有的架构，因为一个所有贡献者都已按同一许可证签署的仓库，很难迁移到另一个许可证。

## 决策

整个仓库采用 GPL-3.0-or-later 许可，这也是 Ansible 和大多数 collection 的许可证。

## 后果

- 贡献者按开发者原创证书（Developer Certificate of Origin）为自己的提交签署。
- 采用不受影响：Ansible 本身就是一个 GPL 命令行工具。
- 当 payload 构建器用 Rust 重写、agent 不再包含 ansible-core 代码时，agent crate 可以改用 Apache-2.0 许可。那将是一个新的决策。

## 更新，2026-09-22

上面的背景描述的是做出这个决策时的项目。自 [ADR 0006](/zh-cn/docs/decisions/0006-warm-python-path/) 起，Volant 把 ansible-core 的 `module_utils` 传送到受管主机，控制节点通过辅助进程而不是嵌入的解释器调用 ansible-core。选择该许可证的理由没有变。

## 更新，2026-10-24

[决策 0009](/zh-cn/docs/decisions/0009-interface-crates-mit-or-apache/) 让引擎保持 GPL-3.0-or-later，并把只描述公共接口的 crate 改为 MIT OR Apache-2.0 许可。它还让 `volant-agent` 和 `volant-protocol` 保持 GPL-3.0-or-later，所以上面后果中提到的 agent crate 可能改换许可证一事，没有计划。
