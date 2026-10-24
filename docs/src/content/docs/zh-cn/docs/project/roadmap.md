---
sourceHash: 12e3021ea96d
title: 路线图
description: Volant 逐个版本的努力方向，以及从哪里接手工作。
---

Volant 处于 pre-alpha 阶段。[Volant 路线图看板](https://github.com/orgs/lorica-labs/projects/1)以 GitHub issue 的形式保存计划，并按版本分组。每个 issue 都写明要构建什么、ansible-core 2.19.12 的行为如何、如何检查结果，以及从代码的哪里入手，所以任何人都可以领取一个。[更新日志](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md)记录实际发布了什么。

## 版本

| 里程碑 | 内容 |
|---|---|
| [v0.1.0-alpha.11](https://github.com/lorica-labs/volant/milestone/2) | 命令套件：用 Rust 实现的 `volant adhoc`、`volant vault` 和 `volant inventory`；通过托管运行时提供的 `doc`、`config`、`galaxy`、`pull` 和 `console`；需主动启用的 `ansible-*` 命令名，让现有脚本无需安装 ansible 就能原样运行 |
| [v0.1.0-alpha.12](https://github.com/lorica-labs/volant/milestone/10) | 安全的切换方式：记录真实的 `ansible-playbook` 运行，在不接触主机的情况下用 Volant 回放，并锁定引擎版本 |
| [v0.1.0-alpha.13](https://github.com/lorica-labs/volant/milestone/3) | 容器连接：docker、podman、kubectl、lxd 与 incus、chroot、buildah、aws_ssm |
| [v0.1.0-alpha.14](https://github.com/lorica-labs/volant/milestone/9) | 内部工作：更短的执行器函数、声明式表格、内存占用 |
| [v0.1.0-alpha.15](https://github.com/lorica-labs/volant/milestone/8) | 网络设备（network_cli、netconf、httpapi）、paramiko_ssh 以及其他 stdout callback |
| [v0.1.0-alpha.16](https://github.com/lorica-labs/volant/milestone/4) | 用 Rust 实现的 `volant galaxy`：collection 与 role |
| [v0.1.0-alpha.17](https://github.com/lorica-labs/volant/milestone/5) | 用 Rust 实现的 `volant pull` 和 `volant console` |
| [v0.1.0-alpha.18](https://github.com/lorica-labs/volant/milestone/6) | Windows 控制节点：`volant.exe` |
| [v0.1.0-alpha.19](https://github.com/lorica-labs/volant/milestone/7) | Windows 主机：winrm、psrp、runas 和 PowerShell 模块 |

[对等性缺口](https://github.com/lorica-labs/volant/milestone/12)列出了目前发现的与 ansible-core 的差异。它们不绑定到某个版本，任何一个都可以随时修复。

## 领取 issue

开始之前请先在 issue 下留言，以免两个人做同样的工作。`size/S` 大约一天，`size/M` 几天，`size/L` 一周或更久，`good first issue` 标记适合入门的任务。[CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md) 介绍环境搭建和各项检查。

缺少什么东西、让你被卡住了？请[提交功能请求](https://github.com/lorica-labs/volant/issues/new?template=feature.yml)，或在 [Discussions](https://github.com/lorica-labs/volant/discussions) 中发起讨论。
