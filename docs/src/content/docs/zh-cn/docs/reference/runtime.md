---
sourceHash: 86490c3c05d2
title: 托管运行时
description: Volant 为自己安装的 Python 和 ansible-core，它们位于哪里、下载什么，以及如何管理。
---

控制节点上需要有一个带 `ansible-core` 的 Python 3，用于 Python 模块、facts 收集，以及 Volant 自己没有实现的过滤器和 lookup。如果机器上没有，Volant 会为自己安装一个，只装一次，之后一直使用它。运行 playbook 不需要安装 ansible。

这个运行时是 CPython 3.13 和 ansible-core 2.19.12，即 Volant 对应的版本。它位于由 Volant 管理的一个目录中，所以不会触碰系统的 Python，也不会触碰你的任何虚拟环境。

## 何时安装

当运行需要 Python 辅助进程却找不到可用的解释器时，会安装这个运行时。它会在标准错误上打印一行，然后继续：

```text
Installing Volant's Python runtime (cpython-3.13.16-ansible-core-2.19.12-98a5d8b2) ...
```

只收集 facts 的运行从不安装任何东西。你也可以提前安装运行时，例如在镜像中：

```sh
volant runtime install
```

在一台干净的机器上、网络正常时，安装用了约 8 秒。之后的运行只需两次文件检查就能找到它。

## 运行使用哪个解释器

按顺序，使用第一个适用的：

1. `VOLANT_PYTHON`，如果已设置。Volant 恰好使用那个解释器，从不安装。
2. 托管运行时，如果已安装。
3. 当前激活的虚拟环境，然后是 `PATH` 中的 `python3`，如果它们带有 ansible-core。
4. 一个新的托管运行时，立即安装。

Volant 用 `-P` 启动解释器，所以工作目录中的文件（例如 `ansible.py` 或 `json.py`）永远不会取代真正的模块被导入。`-P` 需要 Python 3.11 或更高版本，ansible-core 2.19 同样需要。你自己的解释器会保留 `PYTHONPATH` 和它的用户 site。[`volant doc`](/zh-cn/docs/reference/cli/doc/) 也按这条规则选择解释器，并在其中运行 ansible-core 自己的 `ansible-doc`。

`volant runtime status` 会显示下一次运行将做出的选择，以及每个候选项和它的来源。[`runtime` 设置](/zh-cn/docs/reference/configuration/#volant)会改变这个顺序。

## 它下载什么

Volant 用 `curl`（缺少 `curl` 时用 `wget`）获取独立的 CPython 构建和 ansible-core 的 wheel，还会运行 `tar`。每个文件的 URL 和 SHA-256 都固定写在 Volant 二进制文件内部，所以每个发布版本都确切知道可以使用哪些文件。

- 每次下载都会在其中任何内容运行之前，对照固定的哈希值检查。不匹配时，Volant 会删除下载内容，并带着两个哈希值停止。
- `pip` 只安装固定的 wheel，使用 `--require-hashes`，不使用索引。
- 安装在最终目录旁边的临时目录中进行，并在检查 `import ansible` 可用之后移动到位。失败的安装会删除它的临时目录。被中断的安装可能留下一个 `.<id>.partial-*` 目录，下一次安装会把它删除。
- 锁文件避免两个 Volant 进程同时安装。第二个进程会等待，然后发现运行时已经装好。
- 每个子进程都带着 `PYTHONNOUSERSITE=1` 运行，并且不带 `PYTHONPATH`，所以你主目录或环境中的包无法影响它。

支持的平台是 x86_64 和 arm64 上的 Linux 与 macOS。在 Linux 上，Volant 会读取它所在机器的 libc（glibc 或 musl）并选择匹配的构建，不论它是为哪种 libc 编译的。Volant 尚未在 macOS 上测试过：没有可用的 Mac。任何其他系统都会被拒绝，消息中会写明该系统，并指向 `VOLANT_PYTHON`。

## 它位于哪里

位于 `$XDG_DATA_HOME/volant/runtime` 下，或 `~/.local/share/volant/runtime`。在 macOS 上，默认是 `~/Library/Application Support/volant/runtime`。每个运行时有自己的目录，以它的标识符命名（Python 版本、ansible-core 版本，以及固定文件列表的一个简短哈希），所以固定内容不同的发布版本会安装在旧版本旁边。一个运行时约占 155 MB。

## Collection

Volant 不会在运行期间安装 collection。要把一个 collection 安装到运行时中：

```sh
volant runtime install --collections community.general ansible.posix:2.2.2
volant runtime install --collections requirements.yml
```

`--collections` 接受名字（每个可以带可选的 `:version`），或以 `.yml` 或 `.yaml` 结尾的 requirements 文件。这些 collection 会放到运行时内部的 `collections/` 中。Volant 在 `ANSIBLE_COLLECTIONS_PATH` 和 `[defaults] collections_path` 的路径之后才查找那里，所以你自己安装的 collection 优先。[`volant galaxy`](/zh-cn/docs/reference/cli/galaxy/) 安装到该路径的第一项，默认是 `~/.ansible/collections`，并把运行时的目录列在最后。

运行时只包含 ansible-core 及其依赖，没有别的东西。插件需要其他 Python 包的 collection（例如 `json_query` 需要的 `jmespath`）在其中无法工作。这类情况请通过 `VOLANT_PYTHON` 使用你自己的环境。

## 命令

| 命令 | 作用 |
|---|---|
| `volant runtime install` | 如果缺少运行时就安装它。 |
| `volant runtime install --collections NAME_OR_FILE...` | 同时安装 collection。 |
| `volant runtime install --from FILE` | 从 `pack` 生成的文件安装，不下载。 |
| `volant runtime install --from FILE --sha256 HASH` | 只有当文件的 SHA-256 为 `HASH` 时才从该文件安装。 |
| `volant runtime pack FILE` | 把已安装的运行时写入一个文件，并打印它的 SHA-256。 |
| `volant runtime status` | 显示模式、下一次运行使用的解释器，以及托管运行时。 |
| `volant runtime path` | 打印托管运行时的 Python。未安装时以退出码 1 退出。 |
| `volant runtime remove [ID]` | 删除当前的运行时，或指定的那个。 |
| `volant runtime remove --all` | 删除所有运行时。 |

### 没有网络的机器

在有网络的机器上打包运行时，复制该文件，然后在相同平台的机器上安装：

```sh
volant runtime pack runtime.tar.gz
# packed runtime.tar.gz
# sha256 3f1c...e9
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9
```

`install --from` 会拒绝为其他平台或其他运行时制作的文件，并同时写明两者。`pack` 会在一行 `sha256` 中打印该文件的 SHA-256。使用 `--sha256 HASH` 时，`install --from` 会在解包之前拒绝哈希值不同的文件，并显示两个哈希值。不使用它时，安装会打印它所用文件的哈希值，方便你自己比对。安装一个包会运行其中的 Python，所以请使用你从制作它的那台机器上得到的哈希值。

## 关闭

在 `ansible.cfg` 的 `[volant]` 段中设置 `runtime = system`，或设置 `VOLANT_RUNTIME=system`。Volant 随后只使用原先就有的东西，缺少解释器时，会用原来的消息加上一条关于运行时的提示来拒绝。设为 `managed` 时，Volant 只使用 `VOLANT_PYTHON` 或托管运行时，缺少运行时就安装它，并忽略 `PATH` 中的 `python3`。见[配置](/zh-cn/docs/reference/configuration/#volant)。
