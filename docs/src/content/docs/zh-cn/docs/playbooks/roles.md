---
sourceHash: e421c9aa034e
title: Role、include 与 import
description: Volant 在哪里查找 role，依赖、参数规范和 role 变量如何工作，以及 import 和 include 如何把其他文件带入 play。
---

本页回答：Volant 在哪里找到 role，它的依赖和变量如何表现，以及 `import_*` 和 `include_*` 如何引入其他文件。Volant 在 play 编译时读取 role 的目录，并把它的 task 插入步骤列表，所以 role 的任何内容都不需要等待主机。

## 查找 role

`roles:` 条目、`meta/main.yml` 中的依赖和 `import_role` 都以同样的方式指定 role 的名字。Volant 按以下顺序查找：

1. `<playbook_dir>/roles`
2. `ansible.cfg` 中的 `roles_path`，或 `ANSIBLE_ROLES_PATH`。两者都没有时，默认值是 `~/.ansible/roles`、`/usr/share/ansible/roles` 和 `/etc/ansible/roles`。
3. `<playbook_dir>` 本身

相对的 `roles_path` 条目，相对于 `ansible.cfg` 所在的目录解析；如果来自 `ANSIBLE_ROLES_PATH`，则相对于工作目录解析。每个条目在使用之前都会转为绝对路径，并去掉 `.` 和 `..`。role 的 `ansible_search_path` 和 `included:` 行显示的是这条路径，与 ansible-core 一致。

像 `acme.demo.hello` 这样由三部分组成的名字，还会在 `ansible_collections/acme/demo/roles/hello` 下查找，先找 playbook 旁边的 `collections/` 目录，再找每个 `collections_path` 条目。

没有人能找到的 role 会让运行以退出码 1 停止，消息会列出所有尝试过的路径。

## role 内部

`defaults/` 和 `vars/` 先按文件读取，再按目录读取：如果 `vars/main.yml` 存在就读它，否则按名称顺序读取 `vars/main/` 里的每个文件。`tasks/`、`handlers/` 和 `meta/` 只接受单个文件，不接受目录。在 role 条目上，`tasks_from`、`vars_from`、`defaults_from` 和 `handlers_from` 可以选择 `main` 以外的文件。

名字是对应目录下的路径，可以带扩展名，也可以不带（`install`、`install.yml`、`sub/install`）。离开该目录的名字，例如 `../../../x` 或目录之外的绝对路径，会让运行停止，并给出 `Failed loading '...' for role (name) as it is not inside the expected role path`。目录内的符号链接无论指向哪里，Volant 都会跟随，与 ansible-core 一致。

role 的 `handlers/` 目录与它的 task 一起读取。在刷新时，role 的 handler 先于 play 自己的 handler 运行。

## 依赖

`meta/main.yml` 中的 `dependencies` 本身就是 role。它们先于请求它们的 role 运行，两个 role 共用的依赖只运行一次。`allow_duplicates: true` 可以为某个 role 关闭这种去重。

去重看的不只是名字。条目的参数、它的 `vars:`、它的 `when:` 和它的 `*_from` 选择，都是它身份的一部分：

```yaml
roles:
  - base                    # runs once...
  - base                    # ...because this is the same entry
  - { role: base, p: v }    # runs again: different parameters
```

## 参数规范

`meta/argument_specs.yml` 会变成一个独立的 task。它在 role 的依赖之后、role 的第一个 task 之前运行，并根据以 tasks 文件命名的规范条目检查 role 的参数。

这个 task 在破折号之后带有 role 的 `short_description`，所以 `--list-tasks` 显示 `Validating arguments against arg spec 'main' - The spec role`。它带有 `always` tag，所以任何 `--tags` 选择都不会丢掉它。

检查按 Ansible 的顺序进行：先是缺少必需参数，然后是类型，然后是超出 `choices` 的值，最后是规范中没有出现的参数。子选项、`aliases`、`default` 和 `mutually_exclusive` 目前还不检查。

## Role 变量

一个 role 贡献三层变量，它们在[优先级顺序](/zh-cn/docs/variables/precedence/)中处于不同的高度：

| 层 | 所处位置 |
|---|---|
| `defaults/main.yml` | 最底层，低于 inventory 自己的变量。 |
| `vars/main.yml` | 高于 play 的 `vars:`，低于 task 的 `vars:`。 |
| role 参数，即 `{ role: base, p: v }` 这样的条目上的自由键 | 高于 facts。role 参数会压过同名的 `set_fact`。 |

:::tip
写在 role 条目上的 `vars:` 不是 role 参数。它作为普通的 task 关键字传递。两种写法看起来很像，所以当 role 读到你没有预料到的值时，请检查你用的是哪一种。
:::

role 的 `defaults` 和 `vars` 对整个 play 可见，所以 `pre_tasks` 和之后的 role 可以读取它们。对于 role 自己的步骤，它的值会重新叠放在最上面。role 参数只留在 role 及其依赖的 role 内部。

依赖还会读取它上层各个 role 的值。它的 `vars` 位于父 role 的 `vars/main.yml` 和它自己的依赖的 `vars/main.yml` 之上，而它自己的依赖又胜过父 role。它的 `defaults` 方向相反：先是它的依赖的，然后是父 role 的，最后是它自己的。它的参数位于父 role 的参数之上，所以没有同名参数的依赖会读取最近的父 role 的参数。

`public: false` 的 `import_role` 会把它们保留在自己内部，而 `include_role` 除非写了 `public: true`，否则也这样处理。见 [Role 选项](#role-选项)。

## import 与 include

有五条语句从另一个文件引入工作。它们分成两类：

| 语句 | 类型 | Volant 何时读取文件 | 列表输出显示什么 |
|---|---|---|---|
| `import_tasks` | 静态 | play 编译时 | 里面的每个 task |
| `import_role` | 静态 | play 编译时 | 里面的每个 task |
| `import_playbook` | 静态 | play 编译时 | 里面的每个 play |
| `include_tasks` | 动态 | 主机执行到这条语句时 | 只有这条语句 |
| `include_role` | 动态 | 主机执行到这条语句时 | 只有这条语句 |

动态 include 是按主机解析的，因为它的文件名可能取决于该主机的变量。新的步骤在 play 运行期间插入到这条语句的正后方。

### 静态 import

`import_tasks` 的路径先在导入它的文件旁边查找，再在 role 自己的 `tasks/` 目录里查找。

`import_playbook` 把另一个文件里的 play 插入到这条语句所在的位置，每个被导入的 play 保留它来源文件所在的目录。它的文件名用 `-e` 变量渲染，并通过[过滤器和 lookup 使用的 Python 路径](/zh-cn/docs/variables/templating/#需要-python-的过滤器测试器和-lookup)渲染，所以 `{{ which }}.yml` 可以搭配 `-e which=sub.yml` 使用，用 `lookup('env', ...)` 或 collection 过滤器构造的名字也可以。此时还没有 inventory，所以名字不能取决于某台主机。读取未定义变量的名字会让加载以退出码 4 停止：`Error processing keyword 'import_playbook': 'nosuchvar' is undefined`。

`import_playbook` 条目上的三个关键字会作用于该文件中的每个 play：

- `tags` 被加到每个被导入的 play 的 tag 上，所以 `--tags` 和 `--skip-tags` 会选择它的 task，列表输出也会显示它们。
- `vars` 被加到每个被导入的 play 的变量上，并且优先于 play 自己的变量。
- `when` 被放到每个被导入的 play 的每个 task、role 和块的条件之前，也放在它收集 facts 的条件之前。它不会对整个 import 只求值一次。条件为假时，仍然会进入每个 play，仍然会为每个 task 和每台主机打印一条跳过信息。Handler 不受条件影响。

ansible-core 2.19 在这个条目上接受的其他关键字，例如 `ignore_errors`、`become` 和 `environment`，在这里不起作用，Volant 以同样的方式忽略它们。任何其他键都会被拒绝。包含模板的 `tags` 或 `when` 也会被拒绝，因为 Volant 在加载 playbook 时还没有变量。

### 动态 include

这条语句自己说的内容里，只有它的 `vars:` 会传入被 include 的文件。这些变量进入[优先级顺序](/zh-cn/docs/variables/precedence/)中 role 参数的那一层，高于 facts、低于 `--extra-vars`，并且会在嵌套 include 中累积。语句自己的 `tags` 到语句为止，它的 `when` 只对这条语句本身求值。

语句周围各层所说的内容，确实会传到被 include 的 task：play、引入该文件的 role 条目，以及它外面的块。它们的 `tags`、`become`、`become_user`、`no_log`、`when`、`environment`、`delegate_to`、`ignore_errors`、`notify` 和 `vars` 适用于每个被 include 的 task，task 自己的值优先于继承的值。这是 ansible-core 2.19 的规则：它跳过动态加载的父级，去问祖父级。继承来的 `check_mode: true` 在被 include 的文件中会被拒绝，而不是被丢弃。

被 include 的 task 落在写下这条语句的那个块和那个部分里，所以块主体里失败的 include 会被该块的 `rescue` 捕获。

Volant 会在运行之前检查 include 带来的内容。到那时，主机已经连接，前面的 task 也已经运行，所以这个版本尚不支持的模块或关键字，已经不能再让整个运行停止。它转而让执行到该语句的那台主机上的 include 语句失败，文件里的任何内容都不会运行，语句外面的 `rescue` 可以捕获这个失败。

在第一次连接之前，Volant 会收集运行中所有 role 的每个 task 文件和 handler 文件里提到的 Python 模块。所以只在 `setup-{{ ansible_os_family }}.yml` 中提到的模块，在 include 运行时已经在主机上了。对于 `roles:` 和 `import_role`，role 的 `tasks/` 或 `handlers/` 下无法解析的文件会在任何连接之前让运行停止。对于 `include_role`，这次提前扫描不会读取该 role：前面的 play 和 task 会先运行，然后运行在这条语句的横幅之后以退出码 4 结束；include 上写了 `when: false` 则会隐藏这个失败。见 [预热的 Python 路径](/zh-cn/docs/internals/python/#工作原理)。

被 include 的文件里，`notify` 没有指向任何 handler 时，运行以退出码 1 结束，消息与预检给出的是同一句话，与 ansible-core 一致。预检就是 Volant 在连接任何主机之前运行的扫描。任何 `ignore_errors` 或 `rescue` 都捕获不了它。见[块与 handler](/zh-cn/docs/playbooks/blocks/#与-ansible-的差异)。

#### 为 include 引入的内容添加关键字

`include_tasks` 和 `include_role` 上的 `apply:` 为这条语句引入的每个 task，以及被 include 的 role 的 handler，添加关键字。它接受块的关键字：`tags`、`when`、`vars`、`become`、`environment`、`ignore_errors`、`delegate_to` 等。import 会拒绝 `apply`，因为你写在它们身上的关键字本来就会被传递。

```yaml
- include_tasks:
    file: install.yml
    apply:
      tags: [install]
      become: true
  tags: [always]
```

`apply` 中的 tag 作用于被 include 的 task，而不是这条语句。使用 `--tags install` 时，只有这条语句带有被选择保留的 tag（这里是 `always`），它才会运行，随后被 include 的 task 才会运行。`apply` 中的 `when` 对每个被 include 的 task 检查，语句上的 `when` 只检查一次。`apply` 中的 `vars` 是块变量，所以语句自己的 `vars:` 优先于它们。

`apply` 中有块所没有的键时，语句运行时会在它的横幅之后让运行以退出码 4 停止，与 ansible-core 一致。不是映射的 `apply` 会让加载停止。

#### Role 选项

使用 `public: true` 时，`include_role` 的 `defaults` 和 `vars` 对这条语句之后运行的一切可见，其他主机也能看到，因为 ansible-core 把公开 role 的列表保存在 play 里，而不是主机里。在这条语句之前运行的 task 读不到它的任何内容，`when: false` 的 include 也不会提供任何内容。

使用 `allow_duplicates: false` 时，对于已经引入过相同 role 的主机，这条语句会显示它的 `included:` 行，但不运行任何东西。当两条语句指定了相同的 role、相同的文件和相同的 `vars:` 时，它们就是相同的；在循环中，还要求相同的循环变量下有相同的 item。`rolespec_validate: false` 会跳过参数规范检查。

这四条语句的每个选项，在 Volant 和 ansible-core 2.19.12 中的读法：

| 语句 | 选项 | 默认值 | Volant |
|---|---|---|---|
| `include_role` | `name`、`role` | 必需 | 可用 |
| `include_role` | `tasks_from`、`vars_from`、`defaults_from`、`handlers_from` | `main` | 可用 |
| `include_role` | `public` | `false` | 可用 |
| `include_role` | `allow_duplicates` | `true` | 可用 |
| `include_role` | `rolespec_validate` | `true` | 可用 |
| `include_role` | `apply` | 无 | 可用 |
| `import_role` | `name`、`role`，以及四个 `*_from` | 同上 | 可用 |
| `import_role` | `public` | `true` | 可用：`false` 把变量保留在 role 内部 |
| `import_role` | `allow_duplicates` | `true` | 可用，在 play 编译时决定 |
| `import_role` | `rolespec_validate` | `true` | 可用 |
| `import_role` | `apply` | 无 | 与 ansible-core 一样被拒绝，`Invalid options for import_role: apply` |
| `include_tasks` | `file` 或自由格式的名字 | 必需 | 可用 |
| `include_tasks` | `apply` | 无 | 可用 |
| `import_tasks` | `file` 或自由格式的名字 | 必需 | 可用 |
| `import_tasks` | `apply` | 无 | 与 ansible-core 一样被拒绝，`Invalid options for import_tasks: apply` |

`public`、`allow_duplicates` 或 `rolespec_validate` 的值不是布尔值时，task 会失败，`import_role` 则让加载停止，消息与 ansible-core 一致。

### include_vars

`include_vars` 产生的是变量而不是步骤，所以它既不是静态的，也不是动态的。它是一个普通的控制节点端模块，读到的内容与 Ansible 一样，排在 `set_fact` 和已注册的结果之下。

| 问题 | 回答 |
|---|---|
| 它读取什么？ | 一个文件，由 `file:` 指定或写成自由格式的路径；或一个目录里的每个文件，由 `dir:` 指定。两种形式接受的选项不同，同时给出两种形式的选项会让 task 失败。 |
| 文件去哪里找？ | 先在 `vars/` 里找，再在目录本身里找，先针对 role，再针对 playbook。绝对路径原样使用。 |
| `dir:` 去哪里找？ | 不搜索。role 的 task 在 role 的 `vars/` 下读取它，如果存在，`dir: vars/x` 指的就是 role 自己的目录。其他任何 task 都相对于包含该 task 的文件所在的目录读取。 |
| 目录按什么顺序读取？ | 先把目录按完整路径的文本排序，再把每个目录里的文件按名称排序。隐藏文件和目录也会读取，指向目录的链接会被跟随。 |
| 两个文件设置了同一个键怎么办？ | 后面的文件会整体替换前面文件的键。 |
| 结果里有什么？ | `ansible_facts` 和 `ansible_included_var_files`，后者按读取顺序列出文件。内容为空的文件不添加任何东西，但仍会被列出。 |
| 用 vault 加密的文件呢？ | 读取它的 task 会像 `no_log` task 一样被隐藏：`-v` 显示被屏蔽的行，读取过它的循环 item 显示 `(censored due to no_log)`。 |
| 检查模式？ | 这个 task 在检查模式下照常运行，与 Ansible 一样。 |
| 什么会失败？ | 目录不存在、路径不是目录、文件没有有效的扩展名，以及文件不是映射，都会让 task 失败，并设置 `message`，与 Ansible 一致。目录中的失败，会让失败目录之前的目录的变量保留在结果里，但失败的 task 不设置任何变量。无法解析或解密的文件会抛出错误，此时 task 没有自己的结果。类型错误的选项会以 Python 自己的话失败。 |

`dir:` 形式的选项：

| 选项 | 含义 |
|---|---|
| `dir` | 要读取的目录。 |
| `depth` | 读取多少层，目录本身算第一层。默认的 `0` 读取所有层。 |
| `files_matching` | 在每个文件名中搜索的正则表达式，不是在路径中搜索。不匹配的文件被排除。 |
| `ignore_files` | 一组正则表达式，或一个按空白字符拆分的字符串。每个表达式锚定在名字的末尾。匹配的文件被排除。 |
| `extensions` | 文件可以使用的扩展名，不带点。默认是 `yaml`、`yml` 和 `json`。比较区分大小写。 |
| `ignore_unknown_extensions` | 排除扩展名不同的文件。不设置时，这样的文件会让 task 失败。 |
| `name` | 把所有内容放在这一个变量下。它对单个文件也有效。 |
| `hash_behaviour` | `merge` 把每个变量叠加到 task 已能看到的同名变量上，映射一直合并到最深层。任何其他值都是替换。 |

### 嵌套上限

32 层深的语句会被拒绝。一个计数器同时覆盖 role 嵌套、role 的 task 里的 `import_role`，以及被导入文件里的 `import_tasks`，因为循环可以穿过这三者的任意组合。

## 与 Ansible 的差异


- 名字里带有 `.` 的 role，会先被当作路径尝试，再被当作 collection 的 role 尝试，所以目录名里带点的 role 目录仍然可以访问。
- `ansible.cfg` 中的 `collections_path` 和 `ANSIBLE_COLLECTIONS_PATH` 的读取方式与 `roles_path` 相同。这个行为没有拿 ansible-core 来测量过。playbook 旁边的 `collections/` 目录则测量过。
- 缺少的 `import_tasks` 文件，会报告 ansible-core 的前两句话，但不含第三句，第三句指的是一个从未被调用的模块的 Python 错误号。
- 写成 task 的 `import_playbook` 会在 play 开始之前被拒绝，而不是在 play 横幅下失败。退出码相同，都是 2。
- 导入自身的文件会被拒绝，退出码为 1，并带有自己的消息。Ansible 会一直递归到解释器放弃，以退出码 250 结束，并带有很长的回溯。
- 失败的参数规范检查，在结果中带有 `"failed": true`，与 Volant 中所有控制节点端的失败一样。


- include 自身的文件会触及这个上限，并作为 task 失败报告，退出码为 2。Ansible 会一直递归到 Python 栈耗尽，以退出码 250 结束。
- role 的 `tasks/` 和 `handlers/` 之外、无法解析的被 include 文件，会让 task 失败，退出码为 2，并有汇总。Ansible 以退出码 4 退出，且没有汇总。汇总能体现的失败更有用，`rescue` 也可以捕获它。
- 循环内失败的 include 会打印通常的 `failed: [host] (item=...)` 行。Ansible 只在标准错误上打印一条 `[ERROR]`。
- `allow_duplicates: false` 在 role 的 task 被引入时，就把该 role 算作该主机已运行。ansible-core 则在主机至少运行过它的一个 task 时才这么算，所以 task 全部被跳过的 role，在那里仍会再次运行。
- `import_role` 上的 `allow_duplicates: false` 在 play 编译时比较各条语句，所以较早那条语句上的 `when` 不会改变某台主机的结果。
