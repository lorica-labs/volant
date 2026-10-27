---
sourceHash: 9fba53e13fc2
title: "在 Volant 内部下载并解包 galaxy 内容"
description: "决策记录 0012。"
sidebar:
  label: "0012 · Galaxy 的 HTTP 与归档"
  order: 12
---

| 状态 | 日期 |
|---|---|
| 已接受 | 2026-10-27 |

## 背景

`volant galaxy` 安装集合和角色，不依赖托管运行时里的 ansible-galaxy。为此它要通过 HTTPS 与 Galaxy 服务器通信，读写 gzip tar 包，克隆 git 仓库，并校验 GPG 签名。ansible-galaxy 2.19.12 用 Python 的 `urllib` 和 `tarfile` 完成前两项，后两项调用 `git` 和 `gpg` 程序。在 2.19.12 上测得：

- HTTP 跟随重定向（下载制品），遵守 `https_proxy`、`http_proxy` 和 `no_proxy`，使用系统 CA 证书库，配置了令牌时发送令牌头。
- `-vvv` 输出 `Downloading <url> to <tmp>` 和 `Validating downloaded file hash <sha256> with expected hash <sha256>`。
- 集合 tar 包按 `FILES.json` 中的列表逐个文件解出，每个文件都与其 `chksum_sha256` 比对。列表中没有的文件不会解出。
- git 来源调用 `git`，签名调用 `gpg`。

在这份记录之前，Volant 没有 HTTP 客户端，也没有归档代码。托管运行时调用外部命令：`runtime/fetch.rs` 运行 `curl`（或 `wget`），再校验固定的 SHA-256；`runtime/pack.rs` 运行 `tar`。下载一个固定文件这就够了。Galaxy 需要更多：API 调用带令牌，但重定向到其他主机时不能带上；分页的 JSON；以及只写入 `FILES.json` 所列文件的解包。

## 选项

| 问题 | 选项 | 优点 | 缺点 |
|---|---|---|---|
| HTTP | 保留 `curl`，与运行时相同 | 不加新 crate；curl 已经读取代理变量和系统 CA 证书库 | 每个请求一个进程；JSON 和分页经过管道；令牌出现在命令行或临时文件中；较旧的 Windows 没有 `curl.exe` |
| HTTP | 进程内的 `ureq` 加 `rustls` | 代理、重定向和令牌规则集中在一处，在所有系统上行为一致；指向 `127.0.0.1` 测试服务器的普通 `http://` 直接可用 | 二进制更大，构建更久（数据见下）；`ring` 要编译 C，musl 构建需要 musl C 编译器 |
| HTTP | 两者都用，放在一个函数后面 | 运行时保留已测试的路径 | 每条规则要测两条路径，而 curl 路径仍无法在跨主机重定向时去掉令牌 |
| tar 和 gzip | 系统 `tar` | 无新增 | 无法在写入前逐个校验文件；GNU 与 BSD 的 `tar` 选项不同；较旧的 Windows 没有 `tar.exe` |
| tar 和 gzip | `tar` crate 加基于 `miniz_oxide` 的 `flate2` | 条目逐个到来，Volant 对每个条目决定是否写入；`miniz_oxide` 已是依赖 | 几个小 crate |
| git 和 gpg | 外部程序 | 与 ansible-galaxy 相同，所以报错也相同 | 需要已安装 |

## 决策

### 进程内 HTTP，使用 `ureq` 和 `rustls`

galaxy 请求通过 `ureq` 3 发出，这是一个阻塞式 HTTP/1.1 客户端。TLS 使用带 `ring` 提供者的 `rustls`，根证书通过 `rustls-native-certs` 从系统证书库读取。规则如下：

- 代理通过 `ureq::Proxy::try_from_env` 从环境读取，它读取 `ALL_PROXY`、`HTTPS_PROXY`、`HTTP_PROXY` 和 `NO_PROXY`，大小写均可。
- 跟随重定向。只有重定向仍在同一主机时才再次发送令牌头（`RedirectAuthHeaders::SameHost`）。
- `--ignore-certs`（`-c`）在本次运行中关闭证书校验，与 ansible-galaxy 相同。
- 服务器 URL 可以是 `http://`，与 ansible-galaxy 相同。测试在 `127.0.0.1` 上启动的假 Galaxy 服务器依赖这一点。

`runtime/fetch.rs` 中的运行时下载暂时保留 `curl`。把它换到同一个客户端是另一项改动，之后 Windows 控制端下载运行时就不需要 `curl.exe`。

不使用 `ureq` 的 `platform-verifier` 特性和默认的 `rustls-webpki-roots` 特性：两者都会引入 `webpki-root-certs` 或 `webpki-roots`，其许可证为 CDLA-Permissive-2.0，`deny.toml` 不接受。`rustls-native-certs` 不需要它们也能读取同一个系统证书库。

### 用 `tar` 和 `flate2` crate 处理归档

归档用 `tar` crate 读写，不启用其默认的 `xattr` 特性，底层是使用 `rust_backend`（即 `miniz_oxide`）的 `flate2`。安装集合时逐个条目读取 tar 包，只有 `FILES.json` 列出且 SHA-256 匹配的条目才会写入。绝对路径、含 `..` 的路径、或指向目标目录之外的链接都会被拒绝，与 ansible-galaxy 一样。构建集合制品时用同样两个 crate 写出 tar 包。

### `git` 和 `gpg` 仍为外部程序

git 来源调用 `git`，签名调用 `gpg`，与 ansible-galaxy 相同。缺少其中之一时，Volant 输出 ansible-galaxy 2.19.12 输出的内容，并以 1 退出。在 `PATH` 中两者都没有时测得：

```text
[ERROR]: Could not find git executable to extract the collection from the Git repository `https://github.com/ansible-collections/ansible.posix.git`: Failed to find required executable "git" in paths: /tmp/emptybin:/sbin:/usr/sbin:/usr/local/sbin
```

```text
[ERROR]: Failed during GnuPG verification with command 'gpg --status-fd=4 --verify --batch --no-tty --no-default-keyring --keyring=/tmp/gtest/k.kbx - /tmp/gtest/cols/ansible_collections/ansible/posix/MANIFEST.json': [Errno 2] No such file or directory: 'gpg'
```

git 报错中的路径是 `PATH` 加上 ansible-core 追加的 `sbin` 目录。galaxy 不使用 `curl`，所以缺少 `curl` 不影响它。

### 要添加的 crate

添加其中任何一个的拉取请求都引用这份记录。

- `ureq` 3.5，`default-features = false`，特性 `rustls-no-provider`：HTTP 客户端，带上面的代理和重定向规则。
- `rustls` 0.23，`default-features = false`，特性 `ring`、`std`、`tls12`、`logging`：TLS，启动时安装一次提供者。
- `rustls-native-certs` 0.8：系统 CA 证书库，不带 CDLA 许可的根证书包。
- `tar` 0.4，`default-features = false`：逐个读写 tar 条目。
- `flate2` 1.1，`default-features = false`，特性 `rust_backend`：基于 Volant 已依赖的 `miniz_oxide` 的 gzip。

## 后果

- 发布二进制增大 1.6 MB，即 13.4%，测量见下。从零构建的时间变化不超出多次运行之间的波动。

- 每条 galaxy 规则（代理、令牌、重定向、证书、解包）都是 Volant 代码，有自己的测试，通过普通 HTTP 针对假 Galaxy 服务器运行。
- Linux musl 发布任务已经安装 `musl-tools`（由 dist 添加），代理程序用 zig 构建，所以 `ring` 的 C 代码不需要在发布中新增步骤。本地 musl 构建需要 `musl-gcc` 或 `cargo zigbuild`。
- Windows 控制端安装集合不需要 `curl.exe` 或 `tar.exe`。

## 测量，2026-10-27

在 4 核 Linux 机器上，从空的目标目录运行 `cargo zigbuild --release -p volant --target x86_64-unknown-linux-musl`，各运行一次。“加入后”添加了上面五个 crate，以及一个下载并解包 tar 包的函数，使链接器保留它们。

| | 加入前 | 加入后 | 差值 |
|---|---|---|---|
| 去符号的二进制 | 12,334,584 字节 | 13,988,792 字节 | +1,654,208 字节（+13.4%） |
| 从零构建 | 546 秒 | 498 秒 | 在波动范围内：加入 crate 的构建先运行，反而更快 |
| `cargo deny check` | 通过 | 通过 | 新增两个重复版本警告：`getrandom`（0.2，供 `ring` 使用）和 `windows-sys` |

musl 目标的构建新增 21 个 crate。
