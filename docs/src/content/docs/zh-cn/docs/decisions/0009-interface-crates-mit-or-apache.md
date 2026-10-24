---
sourceHash: dad3d3f2ec02
title: "引擎保持 GPL，接口 crate 采用 MIT OR Apache-2.0"
description: "决策记录 0009。"
sidebar:
  label: "0009 · 接口许可证"
  order: 9
---

| 状态 | 日期 |
|---|---|
| 已接受 | 2026-10-24 |

## 背景

其他程序想要驱动 Volant，并读取一次运行做了什么：CI 系统、编辑器、仪表板和 AI agent。目前它们只能解析人类可读的输出，这种输出遵循 Ansible 的文本格式，是写给人看的。计划做一个有文档的事件流，配一个小 crate `volant-events`，里面放它的类型和 JSON Schema，这样 Rust 程序就不必再定义一遍。

[决策 0001](/zh-cn/docs/decisions/0001-gpl-3-0-or-later-license/) 把整个仓库放在 GPL-3.0-or-later 之下。在这个许可证下，任何链接 `volant-events` 的程序也都必须是 GPL，这会让大多数工具无法使用一个本该面向所有人的接口。

## 决策

引擎保持 GPL-3.0-or-later。这涵盖 `volant`、`volant-agent`、`volant-protocol`，以及仓库中除下面这些 crate 之外的一切。

唯一用途是向其他程序描述公共接口的 crate，采用 MIT OR Apache-2.0 许可。第一个会是 `volant-events`。

其他程序通过命令行和有文档的事件流与 Volant 对话。它们不链接引擎的 crate。

## 后果

- 引擎仍然是自由软件。这份记录是项目的承诺：它不会转向专有或源码可见的许可证。
- 任何许可证下的程序都可以读取 Volant 的事件，通过 `volant-events`，或仅凭 JSON Schema。
- 接口 crate 在自己的 `Cargo.toml` 中设置 `license = "MIT OR Apache-2.0"`，而不是继承工作区的许可证，并在自己的目录中保留 `LICENSE-MIT` 和 `LICENSE-APACHE`。根目录的 `LICENSE` 仍涵盖仓库的其余部分。
- 对接口 crate 的贡献按 MIT OR Apache-2.0 接受，与其他每项贡献一样，按开发者原创证书签署。
- 接口 crate 为其用途而写，不取用任何 GPL crate 的代码。
- `volant-protocol` 保持 GPL。把它改成另一个许可证，需要所有为它做过贡献的人同意。
