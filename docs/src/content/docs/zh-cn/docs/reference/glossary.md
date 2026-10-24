---
sourceHash: 725d6c347d6e
title: 术语表
description: 本文档中含义精确的词。
---

### Action plugin

ansible-core 中在控制节点上运行的代码，它决定为 `copy` 或 `package` 这类模块向主机发送什么。Volant 用 Rust 运行其中八个，其余的通过桥接运行。见 [Action plugin](/zh-cn/docs/internals/action-plugins/)。

### Agent

Volant 上传到每台受管主机并缓存在那里的一个小型静态程序。它从控制节点接收 task，运行它们，并把结果以流的形式送回。见[连接与 become](/zh-cn/docs/hosts/connections/)。

### 批次

一台主机在下一次等待其他主机之前连续运行的 task。默认情况下，一个批次只有一个 task 那么宽。见[跨主机批处理](/zh-cn/docs/hosts/batching/)。

### 桥接

Volant 没有某个 action plugin 的 Rust 版本时，该 plugin 走的路径：ansible-core 自带的这个 plugin 的 Python 代码在控制节点的辅助进程中运行，控制节点回答它的请求。见[桥接](/zh-cn/docs/internals/action-plugins/#桥接)。

### 控制节点

你运行 `volant` 的那台机器，也指 `volant` 程序本身。它读取 playbook，渲染模板，并驱动各个 agent。

### 刷新点

被 notify 的 handler 运行的一个步骤。每个 play 有三个，`meta: flush_handlers` 会增加更多。见 [Handler](/zh-cn/docs/playbooks/blocks/#handler)。

### 链路

从控制节点到一个 agent 的一条已打开的连接。一台主机有自己的链路，提权期间多一条，每个委派目标再各多一条。

### 托管运行时

Volant 为自己安装的 Python 和 ansible-core，优先于系统上的任何解释器使用。见[托管运行时](/zh-cn/docs/reference/runtime/)。

### 原生模块

agent 自带的模块的 Rust 版本。当 task 的参数没有超出它所实现的范围时，它自己回答该 task，否则把 task 交还给 Python 模块。见[原生模块](/zh-cn/docs/internals/python/#原生模块)。

### 预检

在任何连接之前，点名指出本版本尚不支持的所有内容并停止运行的检查。见[尚不支持的内容](/zh-cn/docs/internals/preflight/)。

### 参照版本

ansible-core 2.19.12，Volant 复现其行为的版本。与它的差异就是 bug，除非某个页面把它列为有意为之。见[决策记录 0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/)。

### 拼接点

一个其后续步骤只有在 play 运行期间才知道的步骤：handler 刷新，或动态的 `include_tasks` 或 `include_role`。新步骤会被插入到那里。见 [import 与 include](/zh-cn/docs/playbooks/roles/#import-与-include)。

### 步骤

play 被编译成的、扁平且带编号的列表中的一项。每台主机走的是同一个列表。见[play 如何运行](/zh-cn/docs/playbooks/how-a-play-runs/)。

### 同步点

一个批次中的每台主机都要等待其他主机的步骤。在默认的 `linear` 行为下，每个 task 都是同步点。

### 并集

一次运行可能发送给主机的模块集合，在第一次连接之前一次性构建。见[预热的 Python 路径](/zh-cn/docs/internals/python/)。

### 并集包

携带一次运行所需的全部 Python 模块以及它们所导入的共享代码的单个归档。它只会发送给每台主机一次。见[预热的 Python 路径](/zh-cn/docs/internals/python/)。

### 预热的 Python 服务

agent 在主机上持续运行的一个 Python 进程。它只加载一次共享的模块代码，并为每个 task 派生一个子进程。
