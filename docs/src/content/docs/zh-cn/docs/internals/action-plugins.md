---
sourceHash: 796d9c2d62e0
title: Action plugin
description: Volant 如何运行 ansible-core 通过控制节点上的 action plugin 运行的模块，用 Rust 或通过 Python 桥接。
---

ansible-core 自带的大多数模块按模块本身运行：控制节点发送 payload，主机运行它，结果送回。少数模块的运行方式不同：由 ansible-core 自己决定发送什么，有时要先向主机询问一些事，而 task 上的模块名只是入口。`package` 就是一例：实际运行的是 `apt` 或 `dnf`，在主机说明它有哪一个之后才选定。`copy` 是另一例：是否有任何东西要传到主机，取决于主机先回报的一个校验和。

Volant 自己运行其中八个：`copy`、`dnf`、`fetch`、`package`、`reboot`、`service`、`template` 和 `unarchive`。每一个都是控制节点上的一个小型状态机：先被问到下一步，再拿到上一步的结果，直到得出 task 自己的结果。它走的每一步，都是本次运行 union 中的一个普通模块，通过该 task 已有的链路发送，每一步一次往返；下文所述 `copy` 的 `stat` 和 `file` 例外。plugin 最终得到的结果，走的是和其他所有模块结果相同的路径，所以主机回答的任何内容，都不会通过别的什么途径进入 playbook 的变量。

task 自己的 `timeout:` 关键字会以这种方式限定整个条目，八个都适用：每个子 task，以及每一次等待主机回来，包括 `reboot` 的重新连接。用完这个时间，task 就在那里结束，并给出 `Task failed: Timed out after <seconds> second(s).`，而不是 plugin 对剩下的时间可能做出的任何别的处理。

ansible-core 同样通过 action plugin 运行的那些名称，以及语料库中各 collection 的每个 action plugin，走的是另一条路：plugin 自己的 Python 代码，来自 ansible-core，在控制节点的辅助进程中运行。见下文的[桥接](#桥接)。这个版本哪些模块以原生方式运行、哪些在控制节点上运行、哪些走预热的 Python 路径，见[模块](/zh-cn/docs/reference/modules/)。

## `copy`

一个控制节点上的文件，或直接写入目标的 `content:`，只在内容不同时才发送到主机。

plugin 先对目标运行 `stat`，除非 `force: false`，否则会要求返回 SHA-1；`force: false` 时它不要求任何校验和，并且无论现有目标里是什么，都保持不动。它把这个校验和与本地字节的校验和比较。目标已经是相同字节时，运行 `file`，设置 task 要求的属性；其他情况则把字节暂存到主机上，再运行一个 `copy` 子 task。只有最后这种情况才会发送内容。`file` 与 `stat` 在同一次往返中发出：只有当 `stat` 找到校验和相同的普通文件时，agent 才运行它，所以已经是相同字节的目标只需一次往返。在 `copy` 或 `template` 的循环里，如果每个元素都有自己的 `dest`，且没有 `until`、`break_when`、`pause`、`timeout`、`async` 或委派，各元素会一起走每一步，放在同一次往返中：对主机已有文件的循环只需一次往返。

`remote_src: true` 会跳过这一切：`copy` 单独运行，使用 task 自己的参数，`src` 指向主机上已有的文件。

超过 16 MiB 的文件不会被整个读入：插件在控制节点上计算它的哈希，并以 16 MiB 的块发送到主机，agent 在块到达时把它们写入磁盘。`unarchive` 也这样发送它的归档。

在向主机询问任何事之前就会被拒绝的情况，信息用的是 ansible-core 自己的措辞：

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>`。Rust plugin 不复制目录，所以该 task 会在桥接上运行，由桥接来复制。
- 在控制节点上不存在的 `src`，被包装为 `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`。

## `template`

在控制节点上渲染的模板，然后交给 `copy` 所用的同一条路径。

plugin 在 `templates/` 下找到模板，用 task 的变量和它自己的分隔符渲染一次，然后像 `copy` 复制 `content:` 那样复制结果：`stat`，然后是 `file` 或暂存的 `copy`。渲染恰好发生一次；没有任何东西会对结果再渲染第二次，所以受管主机贡献的值会作为文本落入文件，而不是作为它自己的模板。

渲染时会在 task 自己的变量之上设置四个变量：`ansible_managed`（除非 task 已经设置了它）、`template_path`（task 所写的 `src`）、`template_fullpath`（在控制节点上找到的路径）和 `template_destpath`（渲染后的 `dest`，见下文）。ansible-core 还会设置的 `template_host`、`template_uid`、`template_run_date` 和 `template_mtime`，这里没有设置。

`trim_blocks` 默认开启，`lstrip_blocks` 默认关闭，与 ansible-core 一致，文件末尾的换行会保留。`newline_sequence` 接受 `\n`、`\r` 或 `\r\n`，转义写法的四个字符与字面写法的读法相同。六个分隔符选项（`variable_start_string` 及其他五个）和模板文件中的 `#jinja2:` 首行，都按 ansible-core 的读法读取，见[渲染模板文件](/zh-cn/docs/variables/templating/#渲染模板文件)。`output_encoding` 可以写出 UTF-8、带签名的 UTF-8、Latin-1、ASCII、cp1252、ISO 8859-15、UTF-16 和 UTF-32，其他任何编码会被指出为尚不支持。`{% include %}` 和 `{% import %}` 会依次在 `ansible_search_path` 的每一项、playbook 目录和模板文件自身所在目录中查找，先找其中的 `templates/`，再找该目录本身，与 ansible-core 相同。

在向主机询问任何事之前就会被拒绝的情况：

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `Block, variable and comment start strings must be different.`
- `TemplateOverrides.<name> must be <class 'str'> instead of <class 'NoneType'>`，针对为空或为 null 的分隔符选项
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8, ... are`
- 在控制节点上不存在的 `src`，用 ansible-core 自己的措辞，这次没有 `AnsibleActionFail` 包装。

## `package`

由主机的包管理器决定实际运行哪个模块。

名称依次来自：`use:`（除非它是 `auto`）；然后是主机的 `ansible_package_use` 变量；然后是模块所运行的那台主机的 `ansible_facts.pkg_mgr`；然后是一个只过滤这一个 fact 的 `setup`，每个 task 都重新运行，从不保留。名称选中哪个模块，就运行哪个，并且参数里去掉 `use`；`setup` 失败会以 `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>` 结束 task，名称解析不到这个版本能分发的任何东西时，会以 `Could not find a matching action for the "<name>" package manager.` 失败。主机报告的名称永远只是这个封闭列表的查找键：`setup`、`apt`、`dnf`、`dnf5`。union 带上全部四个后端，而不是只带一次运行最终需要的那一个，因为 facts 已知之后不会再构建任何东西：plugin 只能在已经传输过去的东西里选。

## `dnf`

仍然是主机的包管理器，这次是在两个模块名之间选择，而不是四个。

后端名称来自 `use:`，`use` 缺失时来自 `use_backend:`；两者同时出现会被拒绝：`parameters are mutually exclusive: ('use', 'use_backend')`。`auto` 和 `yum` 读取模块所运行的那台主机的 `ansible_facts.pkg_mgr`，在它还不知道时，通过一个只过滤这一个 fact 的 `setup` 收集；其他任何名称按给定的使用。`yum`、`yum4` 和 `dnf4` 都运行 `dnf` 模块，`dnf5` 运行 `dnf5`，仍然不是这些之一的名称会以 ansible-core 自己的两行信息失败，连结尾的大括号也原样保留：

```text
Could not detect which major revision of dnf is in use, which is required to determine module backend.
You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})
```

`setup` 失败时，task 带着它自己的原因结束，即 `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`。task 没有被委派时，它的结果带有 `ansible_facts.pkg_mgr`，设为 `setup` 的回答，与 ansible-core 自己的结果一样；被委派的 task 的结果则没有，因为否则委派目标的回答会落到错误主机的 facts 下。

在 ansible-core 2.19 中，`yum` 模块本身不再是一个独立的模块：`yum`、`ansible.builtin.yum` 和 `ansible.legacy.yum` 都是指向 `dnf` action 的重定向，指名其中之一的 task 会运行这个 plugin。`yum` 的 `module_defaults` 键适用于 `dnf` task，反过来也一样。

## `fetch`

从主机读取的文件，只写到控制节点上 `dest` 之下，不写到别处。

没有 `become` 时，plugin 先对源文件运行 `stat`，并保持已经匹配的本地文件不动；有 `become` 时，它直接用 `slurp`。字节通过 `slurp` 传回，严格解码，从不渲染，也从不放入变量。超过 2 MiB 的文件接近 `slurp` 能携带的上限，改为按每块最多 16 MiB 读回，直接写入目标旁边的文件。有 `become` 时，这发生在 `slurp` 报告文件过大之后，并先用一次 `stat` 取得其校验和。目标是 `dest/<inventory_hostname>/<src>`，除非 `flat: true` 直接把 `dest` 本身作为目标，或者在 `dest` 带有末尾斜杠时，目标是 `dest/<src 的 basename>`。

在 ansible-core 2.19.12 上的实测表明，用 `..` 向上爬的相对 `src` 会让 ansible-core 写到控制节点上 `dest` 之外：不带 `flat` 的 `src` 为 `../../../../../../../../tmp/x` 时会写出 `/tmp/x`。这里每个候选路径都在不接触文件系统的情况下构建并规范化，除非位于 `dest` 之下，否则会被拒绝，这发生在创建任何目录之前：

- `the fetched path <p> is outside '<dest>'`

这会检查两次：一次针对 playbook 所写的 `src`，一次针对主机回报的路径，后者不必与前者一致。开启保护之后，渲染读取了受管主机内容的 `dest` 会在询问任何事之前被拒绝，除非 `[volant] host_named_dest_dirs` 覆盖了它（见[受管主机指定的 dest](#受管主机指定的-dest)）；当某个主机设置了循环的 `inventory_hostname` 时，同样会被拒绝。不是单个路径组成部分的 `inventory_hostname` 则始终会被拒绝：

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

`dest` 之下任何位置的符号链接都会被拒绝，而不是被跟随，playbook 所构建的路径和主机回报的路径都是如此。

`fail_on_missing` 默认开启，与 ansible-core 一致，源文件缺失或为目录时让 task 失败；`fail_on_missing: false` 会把这两种情况变成携带 ansible-core 自己信息的 `ok` 结果。无论哪种，其他任何失败仍会让 task 失败，例如主机拒绝的读取。`validate_checksum` 会在保留写出的文件之前，用主机的校验和检查它的 SHA-1；不匹配会让 task 失败并删除已写出的内容，这与 ansible-core 不同，后者会把不匹配的文件留在写出的地方。被替换的文件保留其现有权限，所以再次 fetch 的 `0600` 文件仍是 `0600`。

### 受管主机指定的 dest

当 `dest` 的渲染读取了主机贡献的内容时，它就是由受管主机指定的。一个真实的例子是 `dest: "{{ tmp.path }}/out/"`，其中 `tmp` 是 `tempfile` task 的 register 结果。ansible-core 会写到那里，`fetch` 默认也会。主机若用 `/home/<operator>/.ssh` 回答 `tempfile`，控制节点就会以操作者的身份写到该目录之下。对一个以 register 的列表为对象、名为 `inventory_hostname` 的循环变量也是如此，它决定了 `dest` 之下的目录。

有两个设置可以开启这项保护，适用于主机不如运行 playbook 的人可信的运行。第一个是 `[volant] host_named_dest_dirs`（环境变量：`VOLANT_HOST_NAMED_DEST_DIRS`），一个以冒号分隔的绝对目录列表，默认未设置。第二个是 `[volant] host_named_paths`，设为 `contained` 或 `refuse`，在 `host_named_dest_dirs` 未设置时它相当于一个空列表，所以每个这样的 `dest` 都会被拒绝。设置了的列表始终生效，不管 `host_named_paths` 怎么说，空值会拒绝每个这样的 `dest`。开启保护后，主机指定的 `dest` 只有在它在文件系统中解析到的路径严格位于列表中某个目录之内时才会被写入：

- `dest` 必须是绝对路径，且不含 `..`。相对路径和以 `~` 开头的路径会被拒绝，永远不会被展开。
- 路径中实际存在的最深部分会经由每一个链接解析，其余部分接在后面。离开该目录的链接、指向无处的链接、目录本身，以及名称以它为前缀的同级目录（对 `/srv/fetch` 而言的 `/srv/fetch-evil`）都会被拒绝。检查在写入之前会再运行一次。
- 写入从列表中的目录开始向下走，会拒绝它之下的任何链接，即使它仍在内部，也会拒绝它之下任何由其他用户拥有、或所有用户都可写且没有粘滞位的现有目录。否则，另一个用户可以在检查和写入之间把其中任何一个换成链接。`fetch` 在途中创建的目录是它自己的。
- 列表中组或其他用户可写的目录，不会在其下写入任何内容，除非设置了它的粘滞位，就像 `/tmp` 那样。这在 task 使用该目录时检查。
- 相对的、为空的、写成 `~` 或 `..` 的条目，或者 `/`，会让运行在启动时以退出码 5 停止。
- playbook 所写的 `dest` 不受列表约束，这个设置对 `src` 也没有任何影响，因为 `src` 指的是主机上的文件。

```
the 'dest' of this task was named by a managed host, and '/home/op/.ssh/x/' is not inside any of the directories (host_named_dest_dirs = /srv/fetch)
```

请指定一个只有操作者能写的目录。列表里有 `/tmp` 时，主机可以在它之下选任何名称。

只有 `fetch` 受这个列表约束。带 `delegate_to: localhost` 的 `copy` 和 `template`，以及拉取模式下的 `synchronize`，也会在控制节点上写一个 `dest`。当主机指定了它时，它们不会被拒绝，与 ansible-core 一致，也与之前一样。

## `service`

主机的初始化系统决定模块，方式与 `package` 的包管理器相同：先是小写的 `use:`（除非是 `auto`），然后是 `ansible_facts.service_mgr`，然后是对 `ansible_service_mgr` 做过滤的 `setup`。与 `package` 不同的是，不属于 `systemd`、`systemd_service`、`sysvinit` 的名称会回退到 `service`，而不是让 task 失败，`setup` 失败时也是如此。直接指名 `systemd` 会去掉该模块不接受的参数：`pattern`、`runlevel`、`sleep`、`arguments`、`args`，每个都带有它自己的警告，`Ignoring "<name>" as it is not used in "systemd"`；而把它写成 `ansible.builtin.systemd` 的 task 会保留这些参数。

同时用到 `package` 和 `service` 的运行，会在任何 fact 已知之前，就把两者的每个后端都带进 union。以 ansible-core 2.19.12 自己的 payload 构建器为基准测得，每条链路一次性发送的内容，比只带 `apt` 和 `systemd_service` 时多约 170 KB。

## `unarchive`

在控制节点上读取、或已经在主机上的压缩包，由 `unarchive` 模块解压。

`copy:` 折叠为 `remote_src`（ansible-core 自己的反转），两者同时出现会被拒绝：`parameters are mutually exclusive: ('copy', 'remote_src')`。主机上已经存在的 `creates:` 会让 task 被跳过，用 `stat` 检查，早于查看 `dest`：`{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}`；失败的 `stat` 被理解为“不在那里”，就像 shell 的存在性测试对一个无法运行的命令的理解那样，而不是 task 失败。目标必须存在且是目录，否则 task 以 `dest '<dest>' must be an existing dir` 失败。只有这之后才搜索源文件，如果它是主机指定的就拒绝，然后暂存并发送，除非 `remote_src` 指向主机上已有的文件。

在向主机询问任何事之前就会被拒绝的情况：

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`，针对 `dest` 或 `creates`。此时 task 在桥接上运行。
- 在控制节点上不存在的 `src`：`Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`，针对在控制节点上被找到为目录的 `src`。

## `reboot`

重启主机，然后等待它以新的 boot id 应答、且它的测试命令成功，之后 task 才结束。

依据 ansible-core 2.19.12 的 `plugins/action/reboot.py` 读出，并与之对照实测，在 `become` 之下、主机自行恢复的情况下依次是：对发行版做过滤的 `setup`，作为裸命令运行的 `cat /proc/sys/kernel/random/boot_id`，跨五个搜索路径查找关机程序的 `find`，关机命令本身，然后反复读取 boot id 直到它改变，最后是测试命令。所有这些都通过 `raw`，经由主机现有的连接运行，方式与 ansible-core 的底层命令在它自己的连接上运行相同。

针对 `local` 运行时，也就是控制节点自己使用的连接，task 会在其他一切之前被拒绝：`Running reboot with local connection would reboot the control node.`

关机命令发出之后，这个版本会丢弃连接，并打开一个新连接来检查主机，而不是在整个重启期间一直持有旧连接。`post_reboot_delay` 是第一次检查之前等待的时间，`reboot_timeout`（默认 600 秒）限定整个等待，除非 task 自己的 `timeout:` 先把它截短。`connect_timeout` 在 task 给出时，只设定每次重新连接自己的 SSH `ConnectTimeout`；它不再限定尝试本身。空的 boot id 应答不会被当作新的启动：主机在即将关闭的那一刻可能什么都不回答。boot id 改变之后，`test_command`（默认 `whoami`）仍必须成功，task 才算完成；任一等待用完都会让 task 以 ansible-core 自己的信息失败，`Timed out waiting for <what> (timeout=<reboot_timeout>)`。同一次运行中，另一台主机保持打开、通向刚重启的那台主机的链路，在下次使用之前会多做一次存活检查，如果重启把它杀死了就会被替换：这个驱动只丢弃它自己的链路，而不丢弃委派目标的链路。

关机命令行遵循 ansible-core 自己的表：大多数发行版是 `shutdown -r <minutes> "<msg>"`，Alpine 上是裸的 `reboot`，Void 上是 `shutdown -r +<minutes> "<msg>"`，task 给出 `reboot_command` 时则在它的第一个空格处拆分。`search_paths`（默认五个目录）是 `reboot_command` 尚未指明绝对路径时查找关机程序的位置；在那里找不到会让 task 失败，并指出搜索过的每个路径。

在向主机询问任何事之前就会被拒绝的情况：ansible-core 自己的九个参数之外的参数（`boot_time_command`、`connect_timeout`、`msg`、`post_reboot_delay`、`pre_reboot_delay`、`reboot_command`、`reboot_timeout`、`search_paths`、`test_command`）会让 task 失败，排序并逐一列出：`Invalid options for reboot: <name>[,<name>...]`。

## 桥接

`add_host`、`assemble`、`gather_facts`、`group_by`、`script`、`set_stats`、`uri` 和 `wait_for_connection` 在辅助进程中运行 ansible-core 自己的 action plugin，collection 自带的每个 action plugin 也是如此（需要 Windows 主机或其他连接类型的 plugin 除外；[对等性](/zh-cn/docs/reference/parity/)页面统计了它们）。`copy` 和 `unarchive` 在 Rust plugin 拒绝的两种情形下走同一条路：目录的 `copy`，以及 `dest` 或 `creates` 里带 `~` 的 `unarchive`。在 `host_named_paths = refuse` 之下，受管主机指定的 `src` 永远不会走这条路：Rust plugin 的拒绝依然成立。`async_status` 不是桥接的 plugin：它由 agent 运行，桥接的 plugin 通过它轮询自己的作业。 以内置名称命名的你自己的 action 插件，放在 playbook 旁的 `action_plugins/`、role 中或 `ANSIBLE_ACTION_PLUGINS` 上时，同样在 helper 中运行：它响应短名称和 `ansible.legacy.<name>`，而 `ansible.builtin.<name>` 仍使用内置插件，与 Ansible 相同。

plugin 说出它需要什么，控制节点来回答：运行这个模块，运行这个命令，读取一个变量，创建一个临时目录，放置这个文件。每个回答都受 task 的 `timeout:` 限定。永不返回的 plugin 或死掉的辅助进程，只会让该 task 失败，而不会让整个运行失败；plugin 留在主机上的临时目录会被删除。`add_host`、`group_by` 和 `set_stats` 不需要主机，不经连接运行。它们返回的内容由执行器应用：为之后的 task 加入新的主机和组，以及在汇总之后输出自定义统计。`add_host` 中由 fact 渲染出的值仍然是数据：它被保留为新主机的 fact，永远不会成为 inventory 变量。`add_host` 或 `group_by` 中由受管主机写下的文本构建出的主机名或组名也是如此。任何含有 `{` 的主机名或组名同样如此，无论它来自 inventory 文件、动态 inventory 来源、`meta: refresh_inventory`、`add_host` 还是 `group_by`：`groups`、`hostvars`、`group_names`、play 的主机列表和 `inventory_hostname` 把它显示为文本，与 ansible-core 一致。一旦存在这样的名称，对携带它的列表的每次读取都是数据。通过 `hostvars[name]` 读取某台主机，只有对那台主机才是数据，所以另一台主机的嵌套模板仍会渲染。

辅助进程代替控制节点的连接，所以 ansible-core 允许 plugin 做的某些事会被按名称拒绝，在它们做任何事之前：

- 不属于 shim 所带的少数几个的连接或 play 上下文成员：`Connection.<name> is not supported yet`、`the connection option <name> is not supported yet`、`the play context field <name> is not supported yet`、`the shell plugin <name> is not supported yet`。
- 除 `status` 和 `cleanup` 之外的 `async_status` 模式。
- shim 按文本读取、却不是 UTF-8 文本的模块。
- 辅助进程内部的 task 关键字 `action` 和 `local_action`。

task 的 `module_defaults` 会传给 plugin。控制节点已经把 action 自己的默认值合并进参数，`gather_facts` 会把其余的默认值用于它运行的 facts 模块，与 ansible-core 一致。

辅助进程的连接有两处遵循 ansible-core：

- `wrap_async` 在 agent 上启动真正的作业。作业并行运行，超出其 `async:` 限制的作业会被杀死，并以 `Timeout exceeded` 结束。
- plugin 以 `sudoable=False` 发送的命令、plugin 的临时目录和它放置的文件，以连接用户的身份运行，经由一条不提权的第二链路。在 `become` 之下，只有 `sudoable` 的命令和模块以提权方式运行。

### 自行建立连接的 plugin

经由 ssh 的 `ansible.posix.synchronize` 并不在主机上运行它的模块。它自己建立一个 `local` 连接，在控制节点上运行模块，由 `rsync -e ssh` 把文件复制到主机或从主机复制回来。辅助进程为这样的 plugin 提供 ansible-core 自己的运行模块的方法，并把它的替身连接留给 task 所带来的那条连接。

plugin 读取链路的密钥和用户（主机变量、`--private-key`、`-u` 或 `ansible.cfg`）、task 是否提权以及以谁的身份（`sudo` 变成 `--rsync-path='sudo -u <user> rsync'`）、`ansible_port`、`delegate_to` 和 `ansible_delegated_vars`，以及在 `use_ssh_args` 下的主机 `ansible_ssh_common_args` 和 `ansible_ssh_extra_args`。`ssh_args` 用的是 ansible-core 的默认值 `-C -o ControlMaster=auto -o ControlPersist=60s`：`ansible.cfg` 中设置的 `ssh_args` 不会被读取。控制节点和主机上都必须有 `rsync`。推送、拉取、`delete`、`rsync_opts`、`delegate_to`、带 `sudo` 的 `become`、`--check` 和 `--diff`，输出与 `ansible-playbook` 的输出一致。

### 已知缺口

- 被委派的 `add_host` 或 `group_by` 作用于 task 自己的主机。

## 受管主机指定的源文件

当 `src` 的渲染读取了主机贡献的内容时，它就是由受管主机指定的：收集到的 fact，如 `src: "{{ ansible_os_family }}.j2"`，register 的结果，或由其构建的 `with_first_found` 的一项。`[volant] host_named_paths` 规定了 `copy`、`template`、`script`、`unarchive` 和 `assemble` 对它如何处理。它适用于在控制节点上读取的文件，在 `--check` 和 `--diff` 下也一样。对于 `template`，值由主机贡献的分隔符选项（`variable_start_string` 和另外五个）在 `allow` 下像 ansible-core 那样使用，在 `contained` 和 `refuse` 下被拒绝。作者模板中的 `#jinja2:` 首行在每种模式下都会被读取。

| 值 | task 的行为 |
|---|---|
| `allow`（默认值） | 查找该文件并发送，与 ansible-core 一致。 |
| `contained` | 只有当文件位于 task 会搜索的 `files` 或 `templates` 目录中时才读取。 |
| `refuse` | 在查找任何路径之前就让 task 失败。 |

`allow` 的意思是，受管主机可以选择由哪个可读的控制节点文件，经 `copy`、`template` 或 `script` task 发送给它，与 ansible-core 2.19.12 完全一样。在那里实测，作为 register 的值、命令的 `stdout` 或 fact 的 `src`，会发送它所指名的任何文件，包括 `~/.ssh`。按 fact 选模板的 role 依赖于这一点。`contained` 和 `refuse` 的存在是为了收紧这一点，适用于主机不如运行 playbook 的人可信的运行。

在 `contained` 之下，算数的目录是：role 的 `files` 和 `templates` 目录、以它为依赖的各个 role 的这两个目录、task 文件所在的目录（role 里是 `tasks/files`），以及 playbook 目录。这些树的其余部分不算：`group_vars`、`vars`、`defaults`、inventory、`ansible.cfg` 和 vault 密码文件永远无法通过主机选定的名称到达，放在 playbook 旁边的文件也不行。规则按顺序如下：

- 以 `~` 或 `~user` 开头的名称会被拒绝。ansible-core 自己的 plugin 会把它展开为主目录，而名称的文本并不显示这一点。
- 含有 `..` 组成部分的名称会被拒绝，即使它折回到内部也一样。文件系统在折叠 `..` 之前会先跟随链接，所以折叠后的路径并不是实际会被打开的那个。
- 绝对名称必须位于这些目录之一的下面，且不能含有 `$`，ansible-core 会在绝对名称里展开它。`with_first_found` 会交出这种绝对名称。
- 相对名称按 plugin 查找它的方式查找。第一个存在的文件就是被读取的那个，它必须位于这些目录之一的下面。playbook 旁边、`files` 之外、与之同名的文件，在它存在时会被拒绝。这样一来，拒绝会告诉主机这样的文件存在。这仅限于位于 `<playbook>/<name>`、`<role>/<name>`、`<role>/tasks/<name>` 或 `<parent role>/<name>` 的文件；对任何其他名称，答案仍是通常的“找不到”。

检查读取的是路径的文本。你自己放在这些目录里的链接是你自己的文件，会被跟随。主机上的源，例如 `copy` 和 `unarchive` 的 `remote_src: true`，或 `remote_src` 未设为 false 时的 `assemble`，不是控制节点上的文件，不会被检查。`template`、`script` 和 collection 的 plugin 不读取 `remote_src`，所以它永远不会让它们豁免。对于 `script`，文件是命令行的第一个词，或 `cmd` 的第一个词。对于 `uri`，`src` 以及 multipart `body` 每个字段的 `filename` 都算数。

当模式或名称的文本就能决定结果时，task 会在查找路径之前被拒绝，因为“找不到”的回答本身就会告诉主机某个路径是否存在于控制节点上。在 `refuse` 之下，信息是：

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

在 `contained` 之下，它会指出这个值和这个设置：

```text
the 'src' of this task was named by a managed host, and '../up.j2' is not inside the
files or templates directories of the playbook or the role (host_named_paths = contained)
```

对 `script` 而言，参数是 `_raw_params` 或 `cmd`。这个设置的其他任何写法都会让运行在启动时以退出码 5 停止，并指出键、三个合法的词和找到的值。

这个设置涵盖上面列出的 task。它不涵盖 `include_vars`、`vars_files`，也不涵盖路径由主机指定的 lookup（如 `file`）。它也不涵盖带 `remote_src` 的 `copy`，或委派给 `localhost`、读取主机指定的控制节点路径的 `slurp`。这些与在 ansible-core 里一样，在 Volant 所有更早的版本里也同样如此。

这个模式改变的是读取哪个文件，从不改变模板的文本如何被信任。当 `template` 的 `src` 由主机指定时，`template_path` 和 `template_fullpath` 在渲染中是该主机的值，就像 `dest` 对应的 `template_destpath` 那样。

`dest` 由主机指定的 `fetch` 也遵循这个设置，尽管它的 `dest` 是控制节点写入的地方，而不是它读取的文件：`allow` 会写入它，`contained` 和 `refuse` 则只在 `host_named_dest_dirs` 之内写入（见[受管主机指定的 dest](#受管主机指定的-dest)）。其他 plugin 的 `dest` 永远只是主机上要写入的位置，所以不会被拒绝；`template_destpath` 把它作为主机自己的值带入渲染。

## 留在主机上的内容

只有当子 task 确实需要时，文件才会传到主机，而且从不比那个 task 走得更远。

控制节点把它暂存为按内容寻址的 blob，以其字节的 BLAKE3 哈希命名，放进一个只属于发送它的那个连接的私有目录：`stage-<host>-<pid>/`，位于 agent 自己的缓存内，权限 0700，除非 agent 独自拥有它，否则会被拒绝。模块从那里读取文件，使用 agent 自己选择的名称，从不使用控制节点上的路径。需要它的子 task 运行之后，无论成功、失败，还是模块本身已经消耗了该文件（例如 `copy` 把暂存的源文件移动到位时），agent 都会删除剩下的部分。没有什么会等到连接结束：渲染出的模板常常带有机密，而一个没人第二次要的文件，不应留着让任何人以后找到。

连接本身结束时，该连接的整个暂存目录也会一起删除，无论里面还剩什么。在主机上新启动的 agent，还会删除它在启动时发现的、同一主机上已死 agent 的暂存目录，所以被中断的运行不会永远留下它们。到同一台主机的两条链路，例如同一台机器的两个 inventory 名称，或委派给同一台主机的两台主机，永远不会共享一个暂存目录，因为每个目录都以各自的进程 id 命名：一个连接暂存的文件，永远不会被另一个连接看到。
