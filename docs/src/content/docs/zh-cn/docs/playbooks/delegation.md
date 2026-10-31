---
sourceHash: 630bc063e9af
title: 委派、meta 任务与 environment
description: 对一个批次只运行一次 task 或在另一台主机上运行，每个 meta 动作的作用，以及 environment 和 no_log 如何影响 task。
---

本页回答如何在 play 当前工作的主机之外的地方运行 task，如何用 `meta` 操控运行本身，以及如何为 task 设置环境变量或隐藏输出。

## run_once

`run_once` 在批次中的一台主机上运行 task，并把它产生的一切（包括它注册的内容）交给其他所有主机。

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

批次中的各台主机会在这样的 task 之前会合，然后才有主机运行它。选举结果必须对所有主机一致，其他主机也必须等待运行它的那一台。

`run_once` 可以是模板。批次中的每台主机依次渲染它，第一台渲染结果为真的主机运行该 task。如果模板在批次内给出不同的答案，运行以退出码 4 结束。

在 `strategy: free` 和 `host_pinned` 下，`run_once` 会被忽略并给出警告，task 在每台主机上运行。失败的 `run_once` task 在那里只让它自己的主机失败。见[其他策略](/zh-cn/docs/playbooks/how-a-play-runs/#其他策略)。

## delegate_to

`delegate_to` 在 task 当前所属主机之外的地方运行它。

```yaml
- name: Take the host out of the load balancer
  command: /usr/local/bin/lb-drain {{ inventory_hostname }}
  delegate_to: lb1
```

task 通过 delegate 自己的连接传送，使用 delegate 自己的连接设置。结果仍然属于 play 正在处理的那台主机：汇总统计的是那台主机，结果行显示为 `[web1 -> lb1]`。

- 被 `when` 跳过的 task 从未到达 delegate，所以它的 `skipping:` 行没有箭头。
- 被委派的 task 不会打开委派方主机自己的连接，所以从不可达主机委派出去的 task 仍然可以运行。
- delegate 在整个 inventory 里查找，而不只是在 play 的主机里。一个针对两台主机的 play，可以委派给第三台主机，而这台主机不会出现在汇总里。
- inventory 里没有的名字，会被当作同名的 SSH 目标，如果连接失败则报告为不可达。不可达的 delegate 会让委派方主机退出运行。

`delegate_facts: true` 把 task 产生的内容存储在 delegate 上，而不是运行它的主机上。

`set_fact` 或 `debug` 这样的控制节点端模块，无论 `delegate_to` 怎么写，始终在控制节点上运行。此时 delegate 只决定结果行上显示的名字，以及在配合 `delegate_facts` 时由谁来写入 facts。

`action:` 和 `local_action:` 接受一个字符串（`command echo hi`）、一个带 `module:` 的映射（Ansible 已弃用这种写法），或者一个单独的模块名，参数放在 `args:` 下。`action:` 之后像 `module=command` 这样的词不指定任何模块。主机执行到它时 task 失败，退出码为 2，`rescue` 或 `ignore_errors` 会像在 Ansible 中一样接住这个失败。

`delegate_to` 和 `delegate_facts` 可以写在块和 task 上，不能写在 play 上，Ansible 也只接受这样的写法。这两个关键字可以组合：带有 `delegate_to` 的 `run_once` task 只运行一次，在 delegate 上运行。

## meta 任务

`meta` 向引擎请求某件事，而不是运行模块。Ansible 2.19 拥有的每一个动作在这里都能运行，每一个都会打印一个 `TASK [meta]` 横幅。Ansible 没有的动作会打印横幅，然后以 `invalid meta action requested: <action>` 和一个 `Origin` 块失败，退出码为 1，即使它的 `when` 为假也是如此。meta task 不会给汇总增加任何内容：它的 `skipping:` 行也不计数。

| 动作 | 作用 | 以谁的 `when` 为准 |
|---|---|---|
| `flush_handlers` | 在 task 所在的位置，运行主机已被通知的 handler。 | 各台主机自己的。 |
| `noop` | 什么也不做。 | 被忽略，并给出警告。 |
| `end_host` | 主机离开 play。不再有 task，没有 `always` 部分，没有 handler。 | 各台主机自己的。 |
| `end_role` | 主机跳过它所在 role 的其余部分，包括该 role 的 `always` 部分。 | 各台主机自己的。 |
| `end_batch` | 批次对所有主机结束，下一个批次开始。 | 第一台主机的。 |
| `end_play` | play 对所有主机结束，剩下的批次也一并结束。 | 第一台主机的。 |
| `clear_facts` | 忘记模块收集的 facts，对批次中的每台主机生效。来自 `set_fact` 的 facts 保留。 | 第一台主机的。 |
| `clear_host_errors` | 放过批次中的失败和不可达的主机。见下文。 | 第一台主机的。 |
| `refresh_inventory` | 重新读取每个 inventory 来源。见下文。 | 被忽略，并给出警告。 |
| `reset_connection` | 关闭主机的连接，同时关闭共享的 `ssh` 连接。下一个 task 会重新打开两者。 | 被忽略，并给出警告。 |

“第一台主机”指批次中仍在 play 里的第一台主机，按 inventory 顺序。读取它的 `when` 的动作只运行一次，打印一个横幅。其他动作对每台主机各运行一次，并为每台仍在 play 中的主机打印一个横幅。

role 之外的 `end_role` 会在运行开始之前让运行停止，退出码为 4，消息与 ansible-core 一致。

### 离开的主机

通过 `end_host`、`end_batch` 或 `end_play` 结束的主机并没有失败，汇总按它当时的状态显示它。在 `rescue` 内部，被 rescue 的失败计为 rescued，主机不影响退出码。已经失败的主机保留它的失败。之后的 play 会再次在这台主机上运行。

### 清除主机错误

`clear_host_errors` 把批次的失败从退出码中去掉。汇总保留它的计数，所以主机可以在退出码为 0 的运行中显示 `failed=1`。

- 失败的主机不会回到它失败的那个 play。它会回到之后的 play。
- 变得不可达的主机会回来。它从停下的地方接着执行 play，并在其他所有主机完成后，运行它错过的步骤。如果它再次不可达，会再次计数。
- 在更早的 play 中不可达的主机也会回来，从执行清除的那个 play 的第一个 task 开始。它在之后的 play 中又会不可达，退出码无论哪种情况都是 4。

### 刷新 inventory

`refresh_inventory` 按 `-i` 的顺序重新读取每个 inventory 来源。`groups` 和 `hostvars` 立刻改变。来源新增的主机从下一个 play 开始加入，来源中少了的主机则在当前位置离开 play，且不算失败。`--limit` 也会重新解析，所以它点名的、只有新来源才有的主机同样会加入。由 `add_host` 和 `group_by` 创建的主机和组会保留。如果来源再也无法读取，inventory 此后就是空的，与 Ansible 一致：每台主机都离开，后面的 play 不匹配任何主机。

facts 和 `set_fact` 的值保持原样。见 [Inventory](/zh-cn/docs/hosts/inventories/)。

## environment

`environment` 为模块运行所在的进程设置变量。

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

各层按键逐个合并，每一层覆盖前一层：先是 play，然后是每个外层的块（由外向内），最后是 task。每一层都用当前循环 item 的变量渲染。

值会变成 Python 的 `str()` 对它们的处理结果，所以 `42` 变成 `"42"`，`true` 变成 `"True"`，`null` 变成 `"None"`。`environment` 内部未定义的变量会让 task 失败。

`environment` 也可以是由映射组成的列表，按顺序合并。值为 `omit` 的条目或值会被省略。

## no_log

`no_log: true` 把 task 的输出替换为 `ansible-playbook` 打印的那一行屏蔽文字，它原本会显示的每一行、每一种详细程度都如此。这涵盖 `ok:` 和 `changed:` 行、`fatal:`、`UNREACHABLE!`、每个循环 item 的行以及 `debug` 的输出。循环 item 的 label 变成 `(censored due to no_log)`。

失败上方打印的 `[ERROR]:` 块保留它的前缀，例如 `Task failed: Action failed:`，并用 `<error censored due to no log>` 取代消息。这里 Volant 与 `ansible-playbook` 2.19.12 不同，后者只屏蔽不可达主机的原因，而完整打印失败 task 的消息，所以 `fail` 消息里的机密会出现在终端上。

`no_log` 可以是模板。每台主机在 task 运行时渲染它。在循环中，只要有一个 item 要隐藏，所有 item 都会被隐藏，而 ansible-core 是按每个 item 自己的值隐藏各个 item。

task 的横幅和 task 的名字不会被隐藏。已注册的变量和汇总保留真实的结果，所以后面的 task 仍然可以读取它。

## 与 Ansible 的差异

- `delegate_to` 按每个 task 渲染一次，而不是按每个循环 item 渲染一次。一个批次就是发给一个 agent、经过一条连接的一条消息，按 item 区分 delegate 会把循环拆散到多条连接上。
- `local_action` 是 `delegate_to: localhost` 的另一种写法，并且优先于同一个 task 上的 `delegate_to`。
- 被选举来运行 `run_once` task 的主机无法连接时，等待的主机会离开 play，运行就此结束。Ansible 会再多打印一个空的 play 横幅。两个引擎都不会选举另一台主机，退出码都是 4。
- `run_once` 与 `delegate_facts` 组合时，facts 存储在 delegate 上，这让 `delegate_facts` 只有一种含义。Ansible 在这里的行为没有测量过。
- 由循环 item 写出的 `become`、`become_user` 和 `become_method` 会按每个 item 渲染。提权方式相同的 item 走同一条连接；提权方式不同的 item 走自己的连接，因此对 `[true, false]` 使用 `become: "{{ item }}"` 时，第一项以 root 运行，第二项以连接用户运行，与 Ansible 相同。这样的循环逐项运行。
- 不是映射的 `environment` 值会被跳过，并给出点明该值的警告。Ansible 则打印它所在的整个层级栈。
- 这条警告不会被 `no_log` 屏蔽。Ansible 的警告同样不被屏蔽，所以这与 ansible-core 一致，但在把机密放进一个可能不是映射的 `environment` 之前，请记住这一点。
