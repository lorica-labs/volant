---
sourceHash: 7aabf754eaae
title: 优先级与 facts
description: 两个来源定义同一个变量时哪个胜出、魔法变量，以及 facts 如何收集、命名和缓存。
---

本页回答两个问题：多个来源设置同一个变量时，变量取什么值；以及主机的 facts 从哪里来。

当两个来源定义同一个变量时，列表中靠下的那个胜出。这个顺序是 ansible-core 2.19 的顺序，其中每个 role 层都与相邻的层逐一测量过。

1. role 的 `defaults/main.yml`
2. `group_vars/all`，先 inventory 目录，再 playbook 目录
3. inventory 的组变量，按组的深度、再按名称应用
4. `group_vars/<group>`，先 inventory 目录，再 playbook 目录
5. inventory 的主机变量
6. `host_vars/<host>`，先 inventory 目录，再 playbook 目录
7. 从主机收集的 facts
8. play 的 `vars`
9. play 的 `vars_files`
10. role 的 `vars/main.yml`
11. task 的 `vars`
12. `include_vars`
13. `set_fact` 和已注册的结果
14. role 参数，以及动态 include 语句的 `vars:`
15. `--extra-vars`

[Role、include 与 import](/zh-cn/docs/playbooks/roles/#role-变量)说明了三个 role 层从哪里来，以及它们有何不同。

只含有 `---` 和注释的变量文件，会加载为空映射，与 ansible-core 一致。内容为标量（例如 `42`）的文件仍会被拒绝。

`vars_files` 的一个条目可以是路径列表。读取列表中第一个存在的文件，其余的忽略。如果其中某个路径用到了没有值的变量，整个条目会被跳过，并给出警告 `skipping vars_files item due to an undefined variable`。

在 Volant 读取的每一种 YAML 文件（变量文件、inventory、playbook 和 task 参数）中，映射都可以用合并键 `<<: *base` 取得另一个映射的键。合并来的键排在前面，写在 `<<` 旁边的键会替换合并来的键。在 `<<: [*a, *b]` 中，第一个映射胜出。不是映射、也不是映射列表的值会让加载停止，消息来自 PyYAML。

:::note
收集到的 fact 比不过 play 自己的变量。playbook 设置了与某个 fact 同名的变量时，读到的是它自己的值，与 Ansible 一致。
:::

## 魔法变量

有一组固定的魔法变量位于每台主机视图的最上层，并且始终胜出：

| 变量 | 内容 |
|---|---|
| `inventory_hostname`、`inventory_hostname_short` | 这台主机在 inventory 中的名字。 |
| `group_names`、`groups` | 这台主机所属的组，以及每个组和它的主机。 |
| `hostvars` | 每台主机的变量，见下文。 |
| `ansible_play_hosts`、`ansible_play_hosts_all` | play 的存活主机，以及它的所有主机。 |
| `ansible_play_batch`、`play_hosts` | 当前 [`serial` 批次](/zh-cn/docs/playbooks/how-a-play-runs/#serial-批次)的主机。`play_hosts` 已弃用。 |
| `playbook_dir`、`inventory_dir`、`inventory_file` | 当前运行的路径。 |
| `omit` | 用来丢弃某个模块参数的占位符。 |
| `vars` | 该主机的全部变量，按名称读取。`vars['a']`、`vars[name]` 和 `vars.get('a')` 会渲染变量中的模板，与 `{{ a }}` 相同。其中不包含 `omit`。 |
| `ansible_playbook_python` | 为 Volant 运行 ansible-core 的 Python 解释器。模板读取它时才去查找，找不到时为未定义。 |
| `ansible_check_mode`、`ansible_diff_mode` | 运行是否带有 `--check` 和 `--diff`。play 或 task 自己的 `check_mode` 或 `diff` 关键字不会改变它们。 |
| `ansible_run_tags`、`ansible_skip_tags` | 运行选择和跳过的 tag，见[检查模式、diff 与列表输出](/zh-cn/docs/playbooks/check-and-diff/#ansible_run_tags-与-ansible_skip_tags)。 |
| `ansible_forks` | 当前的 `forks` 值。 |
| `ansible_version` | Volant 所复现的 ansible-core 版本，见[决策记录 0003](/zh-cn/docs/decisions/0003-ansible-version-reports-the-reference-release/)。 |
| `volant_version` | Volant 自己的版本。用 `volant_version is defined` 可以区分这两个引擎。 |

在 `rescue` 内部，`ansible_failed_task` 和 `ansible_failed_result` 作为 facts 设置在失败的那台主机上。

每个 task 还会得到 `ansible_search_path`，role 的 task 会得到 `role_path`。两者都在 task 自己的 `vars:` 解析之前就位，所以用 `lookup('template')` 构造的 task 变量能找到 role 的 `templates/`。[模板](/zh-cn/docs/variables/templating/#可用内容)列出了搜索路径包含哪些目录。

## hostvars

`hostvars` 保存 inventory、`--extra-vars`、`set_fact` 和收集到的 facts 为每台主机产生的内容。它不包含另一台主机的 play 变量或 task 变量。只要有 fact 发生变化，它就会重建。

每台主机读取的是同一份共享的映射，而不是自己的副本，所以引用 `hostvars` 的 task 付出的是一次查找的代价，而不是复制一遍 inventory。

读取 `hostvars`、`ansible_play_hosts` 或 `ansible_play_batch` 的 task，会先等待每台主机都到达它，这样它看到的就是 Ansible 在那一时刻会显示的内容。

## Facts

facts 是主机报告的关于自己的信息，例如它的名字、发行版和地址。

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

如本页顶部的列表所示，收集到的 facts 的优先级高于 inventory 变量，低于 play 自己的变量。

### 何时收集 facts

除非设置了 `gather_facts: false`，play 都会收集 facts。`ansible.cfg` 中设置 `gathering = explicit` 时，只有写了 `gather_facts` 的 play 才会收集。设置为 `smart` 时，在 [facts 缓存](/zh-cn/docs/hosts/inventories/#facts-缓存)中有新鲜 facts 的主机会被跳过。`gather_facts` 模板在 play 开始时渲染一次。

Volant 或者通过[预热的 Python 路径](/zh-cn/docs/internals/python/)运行 ansible-core 自己的 `setup` 模块，或者在能证明 playbook 不读取其他任何内容时，让 agent（Volant 放在每台主机上的小程序）自己收集 facts。见[原生 facts](#原生-facts)。

由于 `setup` 总是可以回退到它的 Python 模块，控制节点需要 ansible-core，主机需要 Python 3 解释器。只为收集 facts 而需要 Python 的运行，不会安装[托管运行时](/zh-cn/docs/reference/runtime/)，所以没有可用 ansible-core 的控制节点，会在第一次连接之前就让收集 facts 的 play 停止，而不是不带 facts 继续运行。不需要 facts 的 play，请设置 `gather_facts: false`。

`gather_subset` 和 `gather_timeout` 可以用在 play 上，也可以与 `filter` 一起用在 `setup` 模块上，与 ansible-core 一致。

### 每个 fact 的两个名字

每个 fact 在两个地方都可以读取：

- 平铺的，带 `ansible_` 前缀：`ansible_hostname`
- 在 `ansible_facts` 里面，拼写与模块返回的一致：`ansible_facts.hostname`

它们是两个独立的变量。之后对 `ansible_hostname` 的 `set_fact` 会遮蔽平铺的那个，而不影响 `ansible_facts.hostname`。Ansible 的行为相同。

### 原生 facts

原生 facts 收集器是用 Rust 编写的版本，在 agent 内部运行，所以主机不需要 Python。它为 Debian 和 Ubuntu 提供。它收集 ansible-core 在 `gather_subset: min` 下运行的 17 个收集器、ansible-core 的 `hardware` 收集器里的处理器和内存 facts，以及它的 `network` 收集器里的默认路由和地址列表。原生收集器不产生的 fact，例如挂载点、设备或每个接口的详细信息，会被省略。

缺少某个 fact，只有在有东西读取它时才有影响。所以在第一次连接之前，Volant 会读取这次运行的每个 play、role、模板和变量定义，并列出它们可能读取的 facts。只有当这些 facts 全都是它能产生的，才使用原生收集器。通过计算出来的名字进行读取，例如 `ansible_facts[item]`、`vars['ansible_' ~ name]`、`lookup('vars', ...)`，或者把整个 `hostvars[host]` 传给过滤器，都可能读到任何 fact，所以这时会保留 ansible-core 的 `setup`。

在原生收集器无法复现的主机上，它也会把 task 交还：它放弃，改由 Python 的 `setup` 模块运行。这发生在其他发行版、启用了 SELinux、`/etc/ansible/facts.d` 里有本地 facts，或者主机名只有 DNS 知道的情况下。

`--facts python` 始终运行 ansible-core 的模块，`--facts native` 始终向 agent 请求。`--profile` 会打印这次运行选了哪一个以及原因。见[命令行](/zh-cn/docs/reference/cli/#运行与输出)。

### facts 是数据

facts 是主机自己说的话，所以它们到达时是不可信的：fact 里的模板仍然是文本，永远不会被渲染。见[来自主机的值绝不会被当作模板](/zh-cn/docs/variables/templating/#来自主机的值绝不会被当作模板)。

### 在多次运行之间保留 facts

设置 `fact_caching = jsonfile` 后，facts 会从一次运行持续到下一次运行，见 [facts 缓存](/zh-cn/docs/hosts/inventories/#facts-缓存)。
