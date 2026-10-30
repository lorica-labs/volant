---
sourceHash: 376899cd0604
title: 测试
description: Volant 如何对照 ansible-core 进行检查，各种已记录的应答，以及如何添加场景。
---

Volant 的行为必须与 ansible-core 2.19.12 一致。因此，测试套件的大部分是一种对比：ansible-core 运行过一次某个东西，它的应答被记录在仓库中，测试则用同样的输入运行 Volant 并进行比较。

## 记录的种类

每种记录都有一个名字，`just golden <kind>` 会重新生成它。

| 类别 | 记录了什么 | 在哪里运行 |
|---|---|---|
| `template` | `crates/volant/tests/golden/cases.yml` 中每个表达式的渲染结果 | 每个 job，通过 `reference.rs` |
| `inventory` | inventory 的主机、变量和组，以及每个模式选中的主机 | 每个 job，通过 `reference.rs` |
| `scenario` | 一次 playbook 运行：退出码和完整的标准输出 | 每个 job，通过 `reference.rs` |
| `shadow` | 选定场景经由 `volant.compat.record` 回调的一次运行，压缩保存在 `crates/volant/tests/golden/shadow` 中；同时会在回调支持的每个 ansible-core 版本下检查回调自带的示例（`just compat-matrix`） | 每个 job，由 `shadow_golden.rs` 重放每份记录，要求没有差异 |
| `module` | 每个原生模块在真实机器上的应答 | `ssh` job 和 `just test-reference`，通过 `reference_modules.rs` |
| `galaxy` | 针对假 Galaxy 服务器（`golden/reference/galaxy_server.py`）运行的一条 `ansible-galaxy` 命令：退出码、标准输出和标准错误、服务器收到的请求，以及留下的文件及其 SHA-256，保存在 `crates/volant/tests/golden/galaxy/<case>/expected.json` | 每个 job，由 `galaxy_golden.rs` 运行，服务器需要 Python 3 |
| `command-names` | `tests/command-names` 中脚本的输出，这些脚本调用 `ansible`、`ansible-playbook`、`ansible-vault`、`ansible-inventory`、`ansible-doc`、`ansible-config`、`ansible-galaxy`、`ansible-pull` 和 `ansible-console` | `command-names` job |

记录中没有任何内容是手工编辑的。修改输入，在安装了 ansible-core 2.19.12 的情况下运行 `just golden <kind>`，然后阅读 diff。在未改动的树上运行它，文件会与原来完全一样。`module` 类别还需要 `sudo`、systemd、apt 和 cron，因为它会在机器上创建一个账户和一个 unit。

`galaxy` 用例是一个包含 `args` 的 `case.yml`，`args` 是 `ansible-galaxy` 之后的参数，其中 `<golden-server>` 代表服务器的 URL，`<golden-tmp>` 代表本次运行的临时目录。服务器自己构建集合和角色，时间和所有者都是固定的，因此同一个归档在任何地方的 SHA-256 都相同。记录中临时目录写作 `<golden-tmp>`，端口写作 `<golden-port>`。用例目录中的其他文件会被复制到命令运行的临时目录中，因此 `-r requirements.yml` 可以指向其中之一。 `galaxy_golden.rs` 把每条记录的请求对服务器重放一遍，并对子命令已原生实现的每个用例运行 `volant galaxy`。

## command-names job

`command-names` job 运行在没有 `ansible` 也没有 Python 的 `debian:stable-slim` 容器中。它用 `volant shims install` 把 `ansible-*` 这些名字链接到静态的 `volant`，然后完全按用户为 ansible 编写的方式运行 `tests/command-names` 的脚本。每个脚本把输出写入一个文件，并与 `tests/command-names/expected/<script>.out` 比较，后者是 `just golden command-names` 录制的 ansible-core 2.19.12 的记录。脚本失败时，该 job 会上传输出文件。

## 添加场景

场景是 `crates/volant/tests/scenarios` 下的一个目录：

1. 创建 `crates/volant/tests/scenarios/<name>/`，把 playbook、inventory 和 playbook 读取的任何文件放进去。
2. 在它们旁边编写 `case.yml`。单次运行写作 `args: [-i, inv.ini, site.yml]`。同一目录的多次运行写作 `runs:`，即由 `{args: [...]}` 组成的列表。
3. 运行 `just golden scenario`。它会在该目录中写出 `expected.json`。
4. 运行 `touch crates/volant/tests/reference.rs`，然后运行测试。场景列表在该文件编译时就固定下来，所以新目录要在重新构建之后才会被看到。

Volant 的每次运行都发生在目录的一份全新副本中，除非 case 另有规定，否则使用 `--forks 1`。测试会比较退出码和完整的标准输出，其中的耗时已被遮蔽。除了 `args` 和 `runs`，`case.yml` 还接受另外六个键：

- `modes` 列出 `check`、`check-diff` 和 `diff`。于是每次运行会按每个模式再进行一次，分别加上 `--check`、`--check --diff` 或 `--diff`，顺序固定，每次运行都会被记录。
- `env` 为这次运行设置环境变量。
- `program: ansible` 用 `ansible` 而不是 `ansible-playbook` 来记录运行，并运行 `volant adhoc` 而不是 `volant playbook`。
- `forks` 把 fork 数提高到一以上。此时各主机的打印顺序可能不同，所以请在 `unordered` 中以正则表达式列出顺序可能不同的那些行。没有 `forks` 而使用 `unordered` 会被拒绝。
- `needs` 指明机器必须具备什么：`ansible-core`、`sudo` 或 `systemd`。需求得不到满足的 case 会被跳过，除非设置了 `VOLANT_PYTHON`，这时跳过会变成失败。
- `links` 把一个路径映射到链接的目标，每份副本都会在该路径上得到这个链接。用它来表示 git 或 cargo 无法保存的链接，例如指向不存在的文件的链接：`cargo package` 会拒绝这样被跟踪的链接。

## 对等性矩阵

`crates/volant/tests/parity.rs` 把 ansible-core 2.19.12 和固定版本的 `ansible` 包中的 collection 所认识的每个名字（关键字、模块、过滤器、测试器、lookup 以及其他种类的插件），与引擎对它的处理情况进行比较。`just parity-reference` 重新生成参照一侧，即 `crates/volant/tests/parity/reference.json`。[对等性](/zh-cn/docs/reference/parity/)、[关键字](/zh-cn/docs/reference/keywords/)和[模块](/zh-cn/docs/reference/modules/)这几个页面由 `just docs-parity`、`just docs-keywords` 和 `just docs-modules` 根据引擎自己的表格生成，其中任何一个过期时，都会有测试失败。

## YAML 语料库

`just yaml-corpus` 用 Volant 的加载器和 ansible-core 的加载器分别加载一个固定语料库中的每个 YAML 文件,并比较两边的值。语料库包括所固定的 `ansible` 包中的集合、ansible-core 自身的 `test/` 目录树,以及 `crates/volant/tests/parity/yaml_corpus.yml` 所列提交上的公共仓库:kubespray、debops、ceph-ansible、openstack-ansible 等,约一万一千个文件。示例 `yaml_dump` 输出 Volant 这一边,并去掉 Volant 自己的编码;`crates/volant/tests/parity/yaml_corpus.py` 按原因分组输出有差异的文件。Volant 把日期读成文本,这一差异不计入。

`crates/volant/tests/parity/yaml_corpus_known.txt` 列出每个已知差异以及跟踪它的 issue。出现其他差异时,该命令失败。`YAML corpus` 工作流每周运行一次,加载器每次改动时也会运行。某个 issue 修复后,请删除对应条目。

## 运行它们

`just check-touched` 是提交 pull request 之前要运行的检查。它会格式化、运行 clippy 和文件大小棘轮检查、运行分支所改动模块的测试，并检查拼写。`just check-local` 做同样的事，但运行所有测试。CI 会运行所有测试。

`just test` 运行不需要主机的全部内容。`just test-reference` 在你所在的机器上运行 module 记录，这需要 ansible-core、sudo 和 systemd。`just ssh-test` 是 `ssh` job 运行的内容：它从 localhost 上的 sshd 开始，也涵盖端到端测试。与其新增测试函数，不如优先在现有测试中添加场景或表格行。
