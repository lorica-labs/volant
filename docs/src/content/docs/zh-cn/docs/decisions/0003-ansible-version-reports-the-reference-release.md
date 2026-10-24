---
sourceHash: d52d376402f0
title: "`ansible_version` 报告参照 ansible-core 版本"
description: "决策记录 0003。"
sidebar:
  label: "0003 · ansible_version"
  order: 3
---

| 状态 | 日期 |
|---|---|
| 已接受 | 2026-09-06 |

## 背景

许多 Galaxy role 会根据 `ansible_version.full` 或 `ansible_version.major` 选择代码路径。Volant 不是 Ansible，但它复现某一个 ansible-core 版本的行为（见 [ADR 0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/)）。不定义 `ansible_version`，会让这些 role 因变量未定义而失败；发明一个 Volant 专属的值，又会把它们引向未经测试的路径。

## 决策

`ansible_version` 保存 Volant 所复现的参照版本，采用 role 所期望的字典形状（`full`、`major`、`minor`、`revision`、`string`）。另有一个单独的 `volant_version` 变量保存 Volant 自己的版本。

## 后果

- 为参照版本编写的 role，其行为与在参照版本下一致。
- 参照版本变动时（[ADR 0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/) 的流程），`ansible_version` 在同一次变更中随之变动。
- 需要区分 Volant 与 Ansible 的 playbook，可以测试 `volant_version is defined`。
