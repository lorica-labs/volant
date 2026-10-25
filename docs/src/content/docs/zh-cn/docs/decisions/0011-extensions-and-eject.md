---
sourceHash: 528668a2f068
title: "只通过可退回 Ansible 的方式扩展 Ansible"
description: "决策记录 0011。"
sidebar:
  label: "0011 · 扩展与导出"
  order: 11
---

| 状态 | 日期 |
|---|---|
| 已接受 | 2026-10-25 |

## 背景

Volant 按 ansible-core 2.19 的方式运行 Ansible playbook（[决策 0002](/zh-cn/docs/decisions/0002-ansible-core-2-19-as-reference/)）。做到这一点之后，它就可以提供 Ansible 所缺少的功能。风险在于，一个项目开始依赖这样的功能后，就再也回不到 ansible-core。团队若知道自己随时可以离开，就更容易采用 Volant。

有两点让离开成为可能。YAML 始终是源：没有东西取代它。每个扩展都有一条规则，把它改写成行为相同的普通 Ansible，并由一个命令执行这条规则。

## 决策

### 两类扩展

Ansible 能够加载的内容，例如过滤器、lookup 和模块，放在 `volant.compat` collection 中。它们在 ansible-core 下原样可用，因此永远不需要导出。

只有 Volant 理解的关键字和语法写作 `volant.<关键字>`，作为其所作用的 play、block 或任务的一个键：

```yaml
- name: one account per user
  volant.loop: "{{ users }}"
  block:
    - ansible.builtin.user:
        name: "{{ item }}"
    - ansible.posix.authorized_key:
        user: "{{ item }}"
        key: "{{ lookup('file', 'keys/' ~ item ~ '.pub') }}"
```

选择单独的键而不是一个 `volant:` 映射，是因为这样每个扩展都是一个可以搜索、可以单独改写的键。ansible-core 会明确拒绝这样的键，在 2.19.12 上实测：`[ERROR]: 'volant.loop' is not a valid attribute for a Block`，并给出行号。因此，使用了扩展的 playbook 永远不会在 ansible-core 下被一知半解地运行。

### 改写契约

每个扩展都附带一条改写为普通 Ansible 的规则。扩展的行为被定义为其改写结果在 ansible-core 2.19.12 下的行为：Volant 不发明语义，它只是更快地运行改写后的形式，同时保持原文可读。

每个扩展还附带一个往返测试。该测试在 ansible-core 2.19.12 下运行改写后的形式，在 Volant 下运行扩展，并比较输出和汇总。两者缺一，扩展就不能合并。

### `volant eject`

```sh
volant eject [PATH...] [--out DIR] [--json]
```

它读取 `PATH`（默认为当前目录）下的 playbook 和角色，把每个使用了扩展的文件改写为所有扩展都已改写的版本。使用 `--out DIR` 时写入该目录，否则就地改写。改写可能会新增文件，例如改写后的循环所包含的任务文件。这些文件放在来源文件旁边的 `_volant/` 下，按任务名和其内容的哈希命名，因此两次导出得到相同的目录树。

在某个上下文中无法精确改写的扩展，例如下文 `free` 策略下的 `run_once`，会被拒绝，并给出 `文件:行号` 和原因，且不写入任何内容。报告列出每个被改写和新增的文件。`--json` 为每个文件输出一个对象：`path`、`action`（`rewritten` 或 `added`）以及被改写的扩展。全部改写成功时退出码为 0，有扩展被拒绝时为 1，有文件无法读取时为 4。

### `volant check --portable`

```sh
volant check --portable [PATH...] [--json]
```

它列出 ansible-core 会拒绝的每一行，给出 `文件:行号` 和所用的扩展。没有这样的行时以 0 退出，否则以 1 退出。`--json` 为每一行输出一个对象：`path`、`line`、`extension`。CI job 可以用它做关卡，`volant eject` 的输出必须通过它。

### 首批扩展

它们按 ansible-core issue 跟踪器上的公开投票选出。

1. **对 block 的循环**（ansible/ansible#13262），在 block 上使用 `volant.loop` 和 `volant.loop_control`。改写把 block 中的任务移到一个被包含的文件中，并对该包含进行循环：

   ```yaml
   - name: one account per user
     ansible.builtin.include_tasks: _volant/one-account-per-user-3f2a.yml
     loop: "{{ users }}"
   ```

   在 2.19.12 上用两台主机实测，`linear` 和 `serial: 1` 下：先展开各次包含，然后依次运行每个元素的任务，每个任务在所有主机上执行。其中的 `when` 按元素生效，`run_once` 每个元素运行一次。在 `free` 策略下，ansible-core 不遵守被包含文件中的 `run_once`：它给出警告并在每台主机上运行该任务。因此，`free` 下 `volant.loop` 中的 `run_once` 会被 `eject` 拒绝，并由 `check --portable` 报告，因为 Volant 遵循改写结果。

2. **把 block 用作 handler**（ansible/ansible#14270）：`handlers:` 中的一个 block，任务可以通过 block 的名称通知它。在 2.19.12 上实测，按名称通知一个 block 会失败：`[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list`。改写把这个 block 变成一个包含其任务的 handler：

   ```yaml
   handlers:
     - name: restart app
       ansible.builtin.include_tasks: _volant/restart-app-91c0.yml
   ```

   在 2.19.12 上实测：被通知的 handler 为每台被通知的主机包含该文件，并按顺序运行其中的任务，就像 block 那样。它不需要新的键，因为 Volant 在 ansible-core 无法使用 block 的地方接受了它，所以它写作普通 Ansible，但同样会被改写。

3. **`collection uninstall`**（ansible/ansible#67759），是 `volant galaxy` 的一个命令而不是语法。它不改变 playbook 中的任何内容，因此不需要改写，也永远不会阻碍导出。

其余的需求留给后续记录：block 上的 `until` 和 `async`（#46203、#28997）没有能保留重试和等待的精确改写；带条件的 `import_playbook`（#34281）、按任务设置的详细程度（#24215）、tag 表达式中的 AND 与 OR（#11396）以及 `ansible_host` 的地址列表（proposals#97）都需要先各自测量。

### Shadow 重放与 lock

[Shadow 重放](/zh-cn/docs/start/switch-without-risk/)重放的是 ansible-core 的记录，而 ansible-core 会拒绝扩展，因此扩展永远不会出现在记录中。想两者兼得的项目，在记录和重放期间保持 playbook 可移植，切换完成后再开始使用扩展。也可以记录 `volant eject` 的输出并重放它。lock 遵循同样的规则：它固定运行项目的引擎，不涉及项目使用了哪些扩展。

## 后果

- 项目可以随时离开 Volant：`volant eject` 交还一棵 ansible-core 能运行的目录树，`check --portable` 证明这一点。
- 每个扩展在合并前都需要一条改写规则和一个往返测试；在改写不精确的上下文中，扩展会被拒绝，而不是近似处理。
- 实现将在以下 issue 中进行，按规模列出：
  - 解析 `volant.` 键并拒绝未知的键（小）；
  - 改写过程，由 `eject` 和往返测试共用（中）；
  - `volant eject`（中）；
  - `volant check --portable`（小）；
  - block 上的 `volant.loop` 和 `volant.loop_control`（中）；
  - 把 block 用作 handler（小）；
  - `volant galaxy collection uninstall`（小）。

## 测量，2026-10-25

ansible-core 2.19.12，两台主机，本地连接，`gather_facts: false`。改写后的循环包含三个任务：`create`、`once`（带 `run_once: true`）和 `key`（带 `when: item != 'bob' or inventory_hostname == 'h1'`），遍历 `[alice, bob]`。

在 `linear` 下，先是各次包含，然后依次是每个元素的任务：

```text
TASK [per user] ****************************************************************
included: _volant/per-user.yml for h1, h2 => (item=alice)
included: _volant/per-user.yml for h1, h2 => (item=bob)
TASK [create] ******************************************************************
ok: [h1] => {"msg": "create alice on h1"}
ok: [h2] => {"msg": "create alice on h2"}
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key alice"}
ok: [h2] => {"msg": "key alice"}
TASK [create] ******************************************************************
...
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key bob"}
skipping: [h2]
```

在 `free` 下，`run_once` 不被遵守：

```text
[WARNING]: Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [once] ********************************************************************
ok: [h2] => {"msg": "once alice"}
```

在 `serial: 1` 下，每个批次都完整运行整个循环，一台主机接一台主机。`handlers:` 中的 block 不能按名称通知，而它的改写结果可以运行：

```text
TASK [change] ******************************************************************
[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list

RUNNING HANDLER [restart app] **************************************************
included: _volant/restart-app.yml for h1, h2
RUNNING HANDLER [stop] *********************************************************
ok: [h1] => {"msg": "stop"}
ok: [h2] => {"msg": "stop"}
RUNNING HANDLER [start] ********************************************************
ok: [h1] => {"msg": "start"}
ok: [h2] => {"msg": "start"}
```
