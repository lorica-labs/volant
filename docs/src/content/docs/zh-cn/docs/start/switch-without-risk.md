---
sourceHash: a9a50a7c4f07
title: 无风险切换
description: 记录真实的 ansible-playbook 运行，在 Volant 中重放，在重放结果一致后再切换。
---

在把一个 playbook 切换到 Volant 之前，你可以先确认 Volant 的做法与 `ansible-playbook` 完全一致。记录你的真实运行，在 Volant 中重放，然后由记录簿告诉你这个 playbook 何时就绪。

重放从不接触任何主机。记录来自一次真实的 `ansible-playbook` 运行。Volant 运行同一个 playbook，对每个本会连到主机的任务都用记录中的结果作答。你的机器上什么都不会改变，`ansible-playbook` 的运行也照常进行。

## 记录运行

记录来自 `volant.compat` collection 中的一个回调。把它安装到 ansible-core 查找 collection 的位置：

```sh
ansible-galaxy collection install \
  "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
```

然后在 `ansible.cfg` 中启用它：

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

此后每次 `ansible-playbook` 运行都会写下一份记录。`{{ timestamp }}` 会替换为运行开始的时间，以 `.gz` 结尾的文件名会被压缩。`VOLANT_RECORD_PATH` 可以通过环境变量设置路径。[运行记录](/zh-cn/docs/reference/recording/)描述了其格式。

记录保存的是 ansible-core 看到的任务结果，因此可能包含解密后的 vault 值和本次运行的 extra 变量。文件以 `0600` 模式创建。不要把记录纳入版本控制或放进公开制品，CI 制品应设置较短的保留期。

## 重放记录

在你的仓库中运行重放，使用被记录的那次运行所用的 playbook 和清单：

```sh
volant shadow replay shadow/run-20261025T093000Z.jsonl.gz
```

重放的运行输出到 stderr。stdout 上的报告列出不同之处，按严重程度从高到低分为五种：`inventory`、`flow`、`loop`、`args` 和 `judgment`。`--json` 为每个差异输出一个对象。没有差异时退出码为 0，至少有一个差异时为 1，记录无法重放时为 4，例如某个 playbook 在记录之后被修改过。详情参见 [volant shadow](/zh-cn/docs/reference/cli/shadow/)。

报告还会说明哪些内容没有比较以及原因。`no_log` 任务只比较其状态。每次运行都会变化的值，例如 `lookup('pipe')`、`lookup('env')` 或 `now()`，在重放中是未知的：由它构成的参数会计入 `unknown:`，而不是报告为差异。

## 判断 playbook 何时就绪

每次重放都会向 `ansible.cfg` 旁边的 `.volant/shadow.jsonl` 追加一行。可以把它纳入版本控制，也可以放在 CI 缓存中。`volant shadow status` 会读取它：

```sh
volant shadow status --require-ready
```

当一个 playbook 最近连续 `shadow_runs` 次（默认 5 次）重放都是干净的，并且比较过的任务至少达到其任务的 `shadow_coverage` 比例（默认 0.9）时，它就是 `ready`。一次 `dirty` 重放或新的 Volant 版本会让连续次数重新开始，修改 playbook 则不会。这两个设置都放在 `ansible.cfg` 的 `[volant]` 小节中：

```ini
[volant]
shadow_runs = 5
shadow_coverage = 0.9
```

在每个 playbook 都就绪之前，`--require-ready` 会以 1 退出，因此可以作为 CI 中的关卡。

## 在 CI 中

这个 GitHub Actions 工作流在每次推送到 `main` 时记录一次运行，在第二个 job 中重放，并把记录簿保存在缓存中。记录作为保留一天的制品传递。

```yaml
name: shadow

on:
  push:
    branches: [main]
  workflow_dispatch:

permissions: {}

jobs:
  record:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - name: Install ansible-core and the recording callback
        run: |
          pipx install ansible-core==2.19.12
          ansible-galaxy collection install \
            "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
      - name: Run the playbook as usual, recorded
        env:
          ANSIBLE_CALLBACKS_ENABLED: volant.compat.record
          VOLANT_RECORD_PATH: shadow/run.jsonl.gz
        run: ansible-playbook -i inventory.ini site.yml
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7
        with:
          name: shadow-recording
          path: shadow/run.jsonl.gz
          retention-days: 1

  replay:
    needs: record
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
        with:
          persist-credentials: false
      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8
        with:
          name: shadow-recording
          path: shadow
      - name: Restore the ledger
        uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        with:
          path: .volant/shadow.jsonl
          key: shadow-ledger-${{ github.run_id }}
          restore-keys: shadow-ledger-
      - name: Install Volant
        run: |
          curl -fsSL https://volant.sh/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Replay the run through Volant
        run: volant shadow replay shadow/run.jsonl.gz
      - name: Keep the ledger, the replay diverged or not
        if: always()
        uses: actions/cache/save@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0
        with:
          path: .volant/shadow.jsonl
          key: shadow-ledger-${{ github.run_id }}
      - name: Gate on the verdict
        run: volant shadow status --require-ready
```

重放出现差异时重放 job 会失败，playbook 就绪之前关卡会失败。无论哪种情况记录簿都会保存，因此连续次数会延续到下一次运行。

## 未就绪时

- `diverged`：最后一次重放发现了差异。报告会写明主机、任务的 `文件:行号`、ansible-core 的做法以及 Volant 的做法。Volant 不应出现的差异就是缺陷。请[报告它](https://github.com/lorica-labs/volant/issues/new?template=compat.yml)，附上报告而不是记录，因为记录可能包含机密。
- `not-ready`，连续次数不足：继续记录和重放。每次干净的重放加一。
- `not-ready`，覆盖率不足：报告会列出从未比较过的任务。它们位于你记录的运行没有走到的分支中，走到这些分支的运行会把它们补上。
