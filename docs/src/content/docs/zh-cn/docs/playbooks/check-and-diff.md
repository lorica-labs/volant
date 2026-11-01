---
sourceHash: 4a0054ff06cb
title: 检查模式、diff 与列表输出
description: --check 和 --diff 做什么，哪些模块能应答它们，tag 如何选择 task，以及不启动运行就描述运行的命令。
---

本页回答如何在运行改动任何东西之前先预览：用 `--check` 试运行，用 `--diff` 查看文件改动，用 tag 选择 task，以及不连接任何主机的列表输出。

`--check` 报告每个 task 会改动什么，但不真正改动。`--diff` 打印模块对文件所做的改动。两者的工作方式都与 `ansible-playbook` 2.19.12 一样，汇总和退出码也一致。

task 关键字优先于命令行选项。`check_mode: false` 让 task 在 `--check` 下真正运行。`check_mode: true` 让正常 play 里的某个 task 成为试运行。`diff: false` 让某个 task 的改动在 `--diff` 下不显示在屏幕上。

`ansible_check_mode` 和 `ansible_diff_mode` 只反映命令行上的 `--check` 和 `--diff`。play 或 task 自己的 `check_mode` 或 `diff` 关键字不会改变它们。

## 什么在哪里运行

无论走哪条路径，检查模式下的 task 都不会改动主机。

Volant 为一部分模块带有原生版本，用 Rust 编写，在 agent 内部运行，agent 就是它上传到每台主机的小程序。当原生版本无法应答某个 task 时，它会把 task 交还：改由模块自己的 Python 代码在主机上运行。

| Task | 会发生什么 |
|---|---|
| `command`、`shell`、`raw` | 被跳过，与 ansible-core 一致。带有 `creates` 或 `removes`（写在命令行里或 `args:` 中）的 `command` 或 `shell` 会被检查：保护条件拦住它时为 `ok`，它会运行时为 `changed`。 |
| `stat`、`setup`、`package_facts`、`service_facts` | 由 agent 应答。它们只读取，所以检查和 diff 都不改动任何东西。 |
| `file`、`lineinfile`、`systemd`、`user`、`group`、`apt` | 交还给主机上模块自己的 Python 代码，它知道自己会改动什么，也知道如何措辞 diff。 |
| `copy`、`template`、`fetch`、`unarchive` 和其他 action 插件 | 由 ansible-core 自己的插件，通过控制节点的 Python 辅助进程（在控制节点上运行 ansible-core 的进程）运行，由它自己计算检查结果和 diff。 |
| 控制节点端模块（`debug`、`set_fact`、`assert`……） | 照常运行。 |
| `package`、`service`、`dnf` | 先为主机选定模块，然后该模块在检查模式下运行。 |

profile（`--profile`）会写出每次交还的原因：“the task runs in check mode”或“the task shows a diff”。

## 没有 Python 的主机

交还需要主机上有 Python。没有 Python 时，每个会改动主机的原生模块在 `--check` 或 `--diff` 下都会失败：`apt`、`user`、`group`、`file`、`lineinfile`、`systemd`、`systemd_service`，以及 copy 插件所运行的 `copy` 模块。请在主机上安装 Python，或者不带这些选项运行 play。只读取的 task（`stat`、`setup`、`package_facts` 和 `service_facts`），以及 `command`、`shell` 和 `raw`，仍然可以工作。

## diff 输出

只有改动了东西的 task 才会打印 diff。`diff` 关闭时，`copy` 和 `template` 不打印任何内容。辅助进程只在打印 diff 时才启动。

## 已知缺口

- `-vvvv` 按 ansible-core 的顺序列出 ansible-core 列出的选项，以及插件设置行。它的 ssh 行显示的是这个引擎通过 `ssh` 运行的命令（探测、上传和到 agent 的链接），而不是 ansible-core 的 `sftp` 上传，本地连接或模块文件不打印任何行。

[对等性页面](/zh-cn/docs/reference/parity/)列出了每个选项和关键字及其状态。

## Tag

`tags` 可以写在 play、role 条目、块或 task 上。编译器会把外层的 tag 折叠进下面的每个 task。然后 `--tags` 和 `--skip-tags` 决定哪些 task 被编译进来，所以 tag 既决定什么会运行，也决定列表输出显示什么。

### tag 规则

Volant 严格应用 Ansible 的规则：

| task 上的 tag | 行为 |
|---|---|
| `always` | 被每个 `--tags` 选中。除非 `always` 本身被跳过，否则能在 `--skip-tags all` 下保留。 |
| `never` | 不包含在 `--tags all` 和 `--tags tagged` 中。只有当选择明确指定它时才会回来。 |
| 没有 tag | 被当作 tag `untagged`，所以 `--tags untagged` 能选中它。 |

`ansible.cfg` 中的 `[tags] run` 和 `[tags] skip`，以及 `ANSIBLE_RUN_TAGS` 和 `ANSIBLE_SKIP_TAGS`，都是默认值。命令行是在它们之上追加，而不是替换它们。

写在 `include_tasks` 或 `include_role` 上的 tag 到这条语句为止：`--tags inc` 运行这个 include，而不运行它引入的内容。而语句周围的 play、role 条目和块的 tag 确实会传到被 include 的 task，所以对带有 `nginx` tag 的 role 使用 `--tags nginx`，会运行该 role include 的文件。Ansible 的行为相同。

### `ansible_run_tags` 与 `ansible_skip_tags`

task 可以通过两个变量读回自己这次运行的 tag 选择：`ansible_run_tags` 保存 `--tags`（未给出时为 `["all"]`），`ansible_skip_tags` 保存 `--skip-tags`（未给出时为 `[]`），两者都已合并了 `ansible.cfg` 的 `[tags] run` / `[tags] skip` 以及 `ANSIBLE_RUN_TAGS` / `ANSIBLE_SKIP_TAGS` 这两个环境变量。

```yaml
- name: Only fetch the kubeconfig once it has been asked for
  fetch:
    src: /etc/rancher/k3s/k3s.yaml
    dest: "{{ kubeconfig }}"
  when: "'kubeconfig' in ansible_run_tags"
```

## 运行前先看一看

有四个选项会读取 playbook 并打印运行会做什么，而不连接任何主机：

```sh
volant playbook -i inventory.ini site.yml --list-hosts
volant playbook -i inventory.ini site.yml --list-tasks
volant playbook -i inventory.ini site.yml --list-tags
volant playbook -i inventory.ini site.yml --syntax-check
```

它们的输出与 Ansible 的一致，连空白字符都相同。测试套件会在一批 playbook 上，把它们与 ansible-core 逐字节比较。

列表输出只遍历块的主体，所以 `rescue:` 或 `always:` 里的 task 不会出现，与 Ansible 一样。

## 与 Ansible 的差异

- 带有多个 tag 的 play 会按排序后打印它们，`--list-hosts` 按 inventory 顺序打印 play 的主机。Ansible 在这两处打印的是 Python 集合，所以它的顺序每次运行都会变。
- 列表输出不解析模块。模块在这个版本中无法运行的 playbook，仍然可以列出，也仍然能通过 `--syntax-check`，所以你可以在能运行它的版本发布之前先读一读某个 role。
- `ignore_errors` 或 `become` 既不是 true 也不是 false 的 task，会让列表输出以退出码 4 退出。Ansible 会列出它并以 0 退出。这个值是格式错误的 playbook，而不是尚未实现的功能。
