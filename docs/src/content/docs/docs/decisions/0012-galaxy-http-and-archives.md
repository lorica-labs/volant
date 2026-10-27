---
title: "Fetch and unpack galaxy content inside Volant"
description: "Decision record 0012."
sidebar:
  label: "0012 · Galaxy HTTP and archives"
  order: 12
---

| Status | Date |
|---|---|
| accepted | 2026-10-27 |

## Context

`volant galaxy` installs collections and roles without the managed runtime's ansible-galaxy. To do that it talks to a Galaxy server over HTTPS, reads and writes gzip tarballs, clones git repositories and checks GPG signatures. ansible-galaxy 2.19.12 does the first two in Python, with `urllib` and `tarfile`, and runs the `git` and `gpg` binaries for the other two. Measured on 2.19.12:

- HTTP follows redirects (artifact downloads), honours `https_proxy`, `http_proxy` and `no_proxy`, uses the system CA store, and sends a token header when a token is configured.
- `-vvv` prints `Downloading <url> to <tmp>` and `Validating downloaded file hash <sha256> with expected hash <sha256>`.
- A collection tarball is extracted file by file from the list in `FILES.json`, each file checked against its `chksum_sha256`. A file the list does not name is not extracted.
- A git source runs `git`, and a signature runs `gpg`.

Before this record Volant had no HTTP client and no archive code. The managed runtime shells out: `runtime/fetch.rs` runs `curl` (or `wget`) and then checks a pinned SHA-256, and `runtime/pack.rs` runs `tar`. That is enough for one pinned file. Galaxy needs more: a token on API calls that must not follow a redirect to another host, paginated JSON, and an extraction that writes only what `FILES.json` lists.

## Options

| Question | Option | For | Against |
|---|---|---|---|
| HTTP | keep `curl`, as the runtime does | no new crate; curl already reads the proxy variables and the system CA store | one process per request; JSON and pagination through a pipe; the token on a command line or in a temporary file; `curl.exe` is missing from older Windows |
| HTTP | `ureq` with `rustls`, in process | the proxy, redirect and token rules sit in one place and behave the same on every system; plain `http://` to a test server on `127.0.0.1` works as is | a larger binary and a longer build (numbers below); `ring` compiles C, so a musl build needs a musl C compiler |
| HTTP | both, behind one function | the runtime keeps its tested path | two paths to test for every rule, and the curl path still cannot drop the token on a cross-host redirect |
| tar and gzip | system `tar` | nothing new | cannot check each file before writing it; GNU and BSD `tar` take different flags; `tar.exe` is missing from older Windows |
| tar and gzip | `tar` crate with `flate2` on `miniz_oxide` | entries come one at a time, so Volant decides for each entry whether to write it; `miniz_oxide` is already a dependency | a few small crates |
| git and gpg | external binaries | ansible-galaxy does the same, so the errors are the same | they have to be installed |

## Decision

### HTTP in process, with `ureq` and `rustls`

Galaxy requests go through `ureq` 3, a blocking HTTP/1.1 client. TLS is `rustls` with the `ring` provider, and the roots come from the system store through `rustls-native-certs`. The rules:

- The proxy comes from the environment through `ureq::Proxy::try_from_env`, which reads `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`, in upper or lower case.
- Redirects are followed. The token header is sent again only when the redirect stays on the same host (`RedirectAuthHeaders::SameHost`).
- `--ignore-certs` (`-c`) turns certificate checking off for the run, as it does in ansible-galaxy.
- A server URL may be `http://`, as in ansible-galaxy. The fake Galaxy server the tests start on `127.0.0.1` relies on this.

The runtime download in `runtime/fetch.rs` keeps `curl` for now. Moving it onto the same client is a separate change, and it would let the Windows controller download the runtime without `curl.exe`.

`ureq`'s `platform-verifier` feature and its default `rustls-webpki-roots` feature are not used: both pull `webpki-root-certs` or `webpki-roots`, licensed CDLA-Permissive-2.0, which `deny.toml` does not accept. `rustls-native-certs` reads the same system store without them.

### Archives with the `tar` and `flate2` crates

Archives are read and written with the `tar` crate, without its default `xattr` feature, over `flate2` on its `rust_backend`, which is `miniz_oxide`. Installing a collection reads the tarball entry by entry and writes an entry only when `FILES.json` lists it and its SHA-256 matches. An entry with an absolute path, a `..` component, or a link that resolves outside the destination is refused, as ansible-galaxy refuses it. Building a collection artifact writes the tarball with the same two crates.

### `git` and `gpg` stay external

A git source runs `git` and a signature runs `gpg`, as in ansible-galaxy. When one is missing, Volant prints what ansible-galaxy 2.19.12 prints and exits 1. Measured with neither tool on `PATH`:

```text
[ERROR]: Could not find git executable to extract the collection from the Git repository `https://github.com/ansible-collections/ansible.posix.git`: Failed to find required executable "git" in paths: /tmp/emptybin:/sbin:/usr/sbin:/usr/local/sbin
```

```text
[ERROR]: Failed during GnuPG verification with command 'gpg --status-fd=4 --verify --batch --no-tty --no-default-keyring --keyring=/tmp/gtest/k.kbx - /tmp/gtest/cols/ansible_collections/ansible/posix/MANIFEST.json': [Errno 2] No such file or directory: 'gpg'
```

The paths in the git message are `PATH` followed by the `sbin` directories ansible-core adds to it. Since `curl` is not used by galaxy, a missing `curl` does not affect it.

### Crates to add

Each pull request that adds one of these cites this record.

- `ureq` 3.5, `default-features = false`, feature `rustls-no-provider`: the HTTP client, with the proxy and redirect rules above.
- `rustls` 0.23, `default-features = false`, features `ring`, `std`, `tls12`, `logging`: TLS, with the provider installed once at start.
- `rustls-native-certs` 0.8: the system CA store, without a CDLA-licensed root bundle.
- `tar` 0.4, `default-features = false`: reads and writes tar entries one at a time.
- `flate2` 1.1, `default-features = false`, feature `rust_backend`: gzip over the `miniz_oxide` Volant already depends on.

## Consequences

- The release binary grows by 1.6 MB, 13.4%, measured below. The clean build time does not change beyond run-to-run noise.

- Every galaxy rule (proxy, token, redirect, certificates, extraction) is Volant code with its own tests, run against the fake Galaxy server over plain HTTP.
- The Linux musl release jobs already install `musl-tools` (dist adds it), and the agents are built with zig, so `ring`'s C code needs no new step in the release. A local musl build needs `musl-gcc` or `cargo zigbuild`.
- A Windows controller can install collections without `curl.exe` or `tar.exe`.

## Measurements, 2026-10-27

`cargo zigbuild --release -p volant --target x86_64-unknown-linux-musl` from an empty target directory, on a 4-core Linux machine, one run each. "With" adds the five crates above and a function that downloads a tarball and unpacks it, so the linker keeps them.

| | Without | With | Difference |
|---|---|---|---|
| Stripped binary | 12,334,584 bytes | 13,988,792 bytes | +1,654,208 bytes (+13.4%) |
| Clean build | 546 s | 498 s | within noise: the build with the crates ran first and was the faster one |
| `cargo deny check` | passes | passes | two new duplicate-version warnings: `getrandom` (0.2, for `ring`) and `windows-sys` |

The build for the musl target gains 21 crates.
