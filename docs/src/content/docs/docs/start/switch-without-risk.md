---
title: Switch without risk
description: Record your real ansible-playbook runs, replay them through Volant, and switch once the replays agree.
---

Before you move a playbook to Volant, you can check that Volant would have done exactly what `ansible-playbook` did. Record your real runs, replay them through Volant, and let the ledger tell you when the playbook is ready.

A replay never touches a host. The recording comes from a real `ansible-playbook` run. Volant runs the same playbook and answers every task that would reach a host with the recorded result. Nothing changes on your machines, and your `ansible-playbook` runs go on as before.

## Record your runs

The recording comes from a callback in the `volant.compat` collection. Install it where ansible-core finds its collections:

```sh
ansible-galaxy collection install \
  "git+https://github.com/lorica-labs/volant.git#/collections/ansible_collections/volant/compat/,main"
```

Then enable it in `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Each `ansible-playbook` run now writes a recording. `{{ timestamp }}` becomes the time the run started, and a name ending in `.gz` is compressed. `VOLANT_RECORD_PATH` sets the path from the environment. [Run recordings](/docs/reference/recording/) describes the format.

A recording holds task results as ansible-core saw them, so it can contain decrypted vault values and the run's extra variables. It is created with mode `0600`. Keep recordings out of version control and out of public artifacts, and give a CI artifact a short retention.

## Replay them

Run the replay in your checkout, with the playbooks and inventory the recorded run used:

```sh
volant shadow replay shadow/run-20261025T093000Z.jsonl.gz
```

The replayed run prints on stderr. The report on stdout lists what differs, in five kinds from the most serious: `inventory`, `flow`, `loop`, `args` and `judgment`. `--json` gives one object per divergence. The exit code is 0 for no divergence, 1 for at least one, and 4 when the recording cannot be replayed, for example because a playbook changed since. [volant shadow](/docs/reference/cli/shadow/) has the details.

The report also says what it did not compare, and why. A `no_log` task is compared on its status alone. A value that changes between runs, such as `lookup('pipe')`, `lookup('env')` or `now()`, is unknown during a replay, so an argument built from one is counted under `unknown:` instead of being reported as a divergence.

## Know when a playbook is ready

Each replay appends a line to `.volant/shadow.jsonl`, beside your `ansible.cfg`. Commit it or keep it in your CI cache. `volant shadow status` reads it:

```sh
volant shadow status --require-ready
```

A playbook is `ready` once its last replays were clean, `shadow_runs` of them in a row (5 by default), and compared at least a share `shadow_coverage` of its tasks (0.9 by default). A dirty replay or a new Volant version starts the streak again. A changed playbook does not. Both settings go in the `[volant]` section of `ansible.cfg`:

```ini
[volant]
shadow_runs = 5
shadow_coverage = 0.9
```

`--require-ready` exits 1 until every playbook is ready, which makes it a gate in CI.

## In CI

This GitHub Actions workflow records a run on every push to `main`, then replays it in a second job and keeps the ledger in the cache. The recording travels as an artifact kept for one day.

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

The replay job fails when the replay diverges, and the gate fails until the playbook is ready. The ledger is saved either way, so the streak carries over to the next run.

## When it is not ready

- `diverged`: the last replay found a divergence. The report names the host, the task's `file:line`, what ansible-core did and what Volant did. A divergence Volant should not have is a bug. Please [report it](https://github.com/lorica-labs/volant/issues/new?template=compat.yml) with the report, not the recording, since a recording can hold secrets.
- `not-ready`, streak too short: keep recording and replaying. Each clean replay adds one.
- `not-ready`, coverage too low: the report lists the tasks never compared. They sit in branches your recorded runs did not take, and a run that takes them adds them.
