---
sourceHash: 65b041000adc
title: "`become` 在 `sudo` 下运行第二个 agent"
description: "决策记录 0004。"
sidebar:
  label: "0004 · sudo 下的 become"
  order: 4
---

| 状态 | 日期 |
|---|---|
| 已接受；活动连接数的上限被 [ADR 0005](/zh-cn/docs/decisions/0005-fork-permits-span-host-local-batches/) 取代，方法列表扩展到了 `su`（见 2026-10-24 的更新） | 2026-09-09 |

## 背景

Ansible 按 task 提权，把每次模块调用都包在 `sudo` 里。Volant 通过每台主机上一个长期存活的 agent 来运行 task，所以包装每个 task 意味着，要么让 agent 为每个模块调用 `sudo`，要么让 agent 在整个 play 期间本身就是有特权的。

## 决策

控制节点为每个（主机，目标用户）组合打开一个 agent 连接。带 `become` 的 task 通过以 `sudo -H <form> -u <user> -- volant-agent` 启动的 agent 运行，其中 `<form>` 是 `-n` 或 `-k -S -p ''`，由下文所述的探测来决定；不带 `become` 的 task 继续使用无特权的 agent。只支持 `sudo`；其他方法尚不支持，会让运行停止。agent 本身不知道提权这回事。

## 后果

- 两个连续 task 之间目标用户的变化会结束当前批次；常见的情形，即整个 play 都在 `become` 之下，只保持一个批次。
- `sudo` 调用的形式由探测来确定，而不是看有没有提供密码。不需要认证的 `sudo` 不会读取它的标准输入，所以在有缓存认证或有 `NOPASSWD` 规则的主机上，写进去的密码会留在管道里，被 *agent* 当作它第一个帧的头部读走。因此控制节点先尝试 `sudo -n`，只有在 `sudo` 自己因缺少认证而拒绝了请求之后，才回退到 `sudo -k -S -p ''`。只凭密码去猜测形式会让链路挂起，而且只发生在时间戳窗口内的第二个提权连接上，这就是探测不可省略的原因。
- 密码在给出时只会一次性写入 `sudo` 的标准输入，永远不会出现在命令行、日志或结果里。携带它的两个类型有手写的 `Debug` 实现，输出 `<redacted>`，所以 `-vvv` 转储不会泄露它，`sudo` 自己捕获的标准错误在进入任何信息之前也会被清除其中的密码。
- 失败的提权是失败的 task，而不是不可达的主机：连接本身是好的。
- 每台主机的活动 agent 连接数没有上限。`forks` 限制的是同时运行的主机数，而不是一台主机持有多少链路，映射表会为每个不同的目标用户保留一条链路直到汇总，每条都是一个独立的 `ssh` 进程。提权链路还会为探测多占一个 `ssh` 连接，需要密码时是两个。这个决策没有增加任何连接复用：每条链路都是它自己的 `ssh` 进程，规模很大的 inventory 要为其中每一条分别付出代价。
- `su`、`doas`、`pbrun` 等可以作为启动 agent 的其他方式加入，无需改动协议。

## 更新，2026-10-24

`su` 自 2026-10-16 起获得支持（[#333](https://github.com/lorica-labs/volant/pull/333)），在 `local` 连接上自 2026-10-23 起获得支持（[#755](https://github.com/lorica-labs/volant/pull/755)）。它启动 agent 的方式与 `sudo` 相同，这正是上面最后一条后果所预期的扩展，不需要改动协议。`doas`、`pbrun` 和其他方法仍被拒绝。两种方法的说明见[权限提升](/zh-cn/docs/hosts/connections/#become)。

此后，活动连接数的上限又变了两次。[ADR 0005](/zh-cn/docs/decisions/0005-fork-permits-span-host-local-batches/) 把它们与 fork 许可绑定在一起。自 2026-09-22 起（[#178](https://github.com/lorica-labs/volant/pull/178)），一台主机持有它自己的链路，外加至多一条提权链路，无论 play 中有多少个目标用户。提权到不同的用户时，会替换手头的提权链路。
