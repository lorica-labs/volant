---
sourceHash: a518b6925ec5
title: 循环与重试
description: loop、with_items 和 loop_control，以及 until、retries 和 delay 如何重复运行一个 task。
---

本页回答如何用 `loop` 和 `with_<lookup>` 对一个列表的各项重复运行 task，以及如何用 `until`、`retries` 和 `delay` 反复运行 task，直到它成功。

## 循环

```yaml
- name: Create the service accounts
  command: useradd --system {{ item }}
  loop:
    - app
    - worker
  loop_control:
    label: "{{ item }}"
  register: accounts
```

`loop` 和 `with_items` 不能同时出现在同一个 task 上。`with_items` 会把嵌套列表展平一层，`loop` 不会，与 Ansible 一致。为带循环的 task 使用 `register` 时，会收集到一个 `results` 列表，每一项对应一个条目。

`loop_control` 读取 `loop_var`、`label`、`index_var`、`pause`、`extended`、`extended_allitems` 和 `break_when`。ansible-core 不认识的子键，会在加载 playbook 时被拒绝。每种 `with_<lookup>` 形式都能运行。见[关键字](/zh-cn/docs/reference/keywords/#under-loop_control)和 [`with_` 循环](#with_-循环)。

## `with_` 循环

`with_<name>` 等价于 `loop: "{{ lookup('<name>', *terms, wantlist=True) }}"`，并且适用于 ansible-core 拥有的每一个 lookup 插件：

| Lookup | 示例 |
|---|---|
| `ansible.builtin` 里的二十五个 | `with_dict`、`with_nested`、`with_subelements`、`with_sequence` |
| ansible-core 重定向到 `community.general` 的 lookup | `with_flattened`、`with_cartesian`、`with_filetree` |
| collection 的 lookup，使用完整名称 | `with_community.general.dependent` |
| playbook 或 role 旁边 `lookup_plugins/` 目录中的插件 | 插件的文件名，写作 `with_<name>` |

内置的 lookup 也可以写成 `with_ansible.builtin.<name>` 和 `with_ansible.legacy.<name>`。

名称是不是 lookup，由控制节点的 ansible-core 在第一次连接之前判断。不是 lookup 的 `with_` 名称会让 playbook 失败，并给出 `'with_xyz' is not a valid attribute for a Task`，与 Ansible 一致。带有两个循环的 task 会失败，并给出 `duplicate loop in task: <name>`，没有值的 `with_` 键则给出 `you must specify a value when using with_<name>`。

抛出异常的 lookup 会让 task 失败，并附上插件自己的语句。Volant 用 Rust 自己运行的 lookup（`file`、`unvault`、`vars`、`pipe`、`template` 和 `first_found`）失败时，会给出 `The lookup plugin '<name>' failed: <reason>` 以及 ansible-core 的错误链，在 `when:` 和模板文件中也是如此。`vars` 的措辞是 `No variable named '<name>' was found.`，没有插件那一部分。整个循环值是一个读取未定义变量的模板时，失败为 `'name' is undefined`。列表中某个元素读取了未定义变量时，失败会使用引擎自己对此的措辞，不含 Ansible 打印的错误链。

## 逐项渲染

循环在处理到某一项时，才渲染这一项的参数、`when`、关键字和 label，而不是在循环开始之前。

- 渲染失败的 item 单独失败，显示为 `failed: [h] (item=2)`。它之后的 item 仍然会运行，task 以 `One or more items failed` 结束。
- 先结束循环的 `break_when`，会隐藏它之后各项的渲染错误。
- 带 `loop_control.pause` 的 `debug` 或 `set_fact` 循环会在暂停之后渲染每一项，因此 `now()` 读到的是该项运行时的时间；循环中的 `set_fact` 仍会把它设置的值交给后面的项。
- 读取未定义变量的 `loop_control.label`，会在模块运行之后让它所在的 item 失败。

## `with_first_found` 和 `with_fileglob`

```yaml
- name: Distribute the matching config
  copy:
    src: "{{ item }}"
    dest: /etc/app/config.yml
  with_first_found:
    - "{{ ansible_distribution }}.yml"
    - default.yml

- name: Copy every image
  copy:
    src: "{{ item }}"
    dest: /var/lib/app/images/
  with_fileglob:
    - "images/*.tar.gz"
```

两者都像任何 `with_<name>` 形式一样，对一个 lookup 做循环：`with_first_found` 逐个检查它的条目，直到某个条目指向一个存在的文件；`with_fileglob` 收集一个 glob 模式的所有匹配项。`with_first_found` 还会在每个搜索条目本身之前，先搜索该条目的一个子目录：当 task 的 action 名为 `template` 时是 `templates/`，名为 `include_vars` 时是 `vars/`，其他情况是 `files/`，也就是普通的 `template:` 或 `include_vars:` task 自己会搜索的那个目录。

什么也找不到时，task 以 ansible-core 自己的语句 `No file was found when using first_found.` 失败，除非该条目是带有 `skip: true` 的映射，此时循环为空运行。渲染为未定义的条目会被丢弃，而不是让整个 lookup 失败；写成单个字符串的循环值渲染出的列表中的未定义值也会被丢弃，这样 role 就可以读取尚无主机设置过的 fact，并仍然走到它的 `when`。任一 lookup 找到的路径如果仍然含有模板标记（`{{`、`{%` 或 `{#`），会按名称被拒绝，而不是再渲染一次。

lookup 在控制节点上找到的路径属于 playbook 自己的内容；部分由主机的值构成的 item，例如读取 `ansible_distribution` 的条目，则作为数据绑定，与对已注册的值做 `loop:` 得到的 item 相同。

## 用 until 重试

```yaml
- name: Wait for the service to answer
  command: curl -fsS http://localhost:8080/health
  register: health
  until: health.rc == 0
  retries: 10
  delay: 3
```

`until` 反复运行 task，直到它的表达式成立。

- `retries` 是第一次尝试之后的重试次数，而不是总次数：task 最多运行 `retries + 1` 次。只写了 `until` 时它是 3，最多运行四次。
- 小于 1 的 `retries` 会关闭重试：task 只运行一次，结果里没有 `attempts`。
- Volant 在前 `retries` 次失败的运行之后各等待 `delay` 秒（默认 5 秒），最后一次之后从不等待。
- 结果的 `attempts` 里记录实际进行的重试次数，最多为 `retries`。重试次数用完的 task 即为失败，即使模块本身成功了。如果有 `failed_when`，则由它在最后一次运行上的结果决定：`failed_when: false` 让 task 通过，而已注册的结果仍然显示 `failed: true`。
- 没有 `until` 的 `retries`，会在 task 的结果为失败时重复运行它。
- 由 action 插件支持的 task（`copy`、`dnf`、`package`……）在每次尝试时，都会用全新的插件，从头重试它的整个子 task 序列，任何文件也会重新发送；`until` 只读取最后一个子 task 的结果。

:::note
默认延迟下重试三次，会在四次运行之间花十五秒等待，与 Ansible 一致。
:::

带循环的 task 对每一项分别重试，并在开始下一项之前完成当前一项的所有尝试。

`until`、`retries` 和 `delay` 在 `include_tasks` 和 `include_role` 上会在第一次连接之前被拒绝。Ansible 在加载 playbook 时也会在这里拒绝它们。`include_vars` 是普通模块，像其他 task 一样重试。

## 与 Ansible 的差异

- 无法求值的 `until` 表达式，会带着 ansible-core 的前缀和 Volant 自己的消息报告，方式与失败的 `when` 相同。
- 因未定义变量以外的原因而失败的 `loop_control.label`，例如抛出异常的 lookup，会在任何 item 运行之前就让 task 失败。ansible-core 则在那一项运行之后才结束 task。
