---
title: "Choose the ansible-core release per project, and keep what Ansible removes"
description: "Decision record 0010."
sidebar:
  label: "0010 · Core version profiles"
  order: 10
---

| Status | Date |
|---|---|
| proposed | 2026-10-25 |

## Context

Volant reproduces ansible-core 2.19.12 ([decision 0002](/docs/decisions/0002-ansible-core-2-19-as-reference/)). Ansible keeps moving: 2.20 and 2.21 are out and 2.22 is in release candidate. Each release removes or changes behaviour that playbooks rely on. Some teams stay on the release they run today, others want the newest behaviour, and a single reference cannot serve both.

On 2026-10-25 the golden corpus was recorded again under ansible-core 2.20.10, 2.21.5 and 2.22.0rc1 and compared with the 2.19.12 recordings, after removing times, job ids and process ids. The corpus covers templates, inventories and 355 scenarios run 1032 times. Fourteen files differ under 2.20, 76 under 2.21 and 94 under 2.22. Most changes are small, such as a key added to a result or a new prefix on an error, but some change a run: a loop result no longer has `skipped`, so a playbook that reads `r.skipped` stops with exit code 2 under 2.21. The [appendix](#appendix-corpus-diff-2026-10-25) lists them.

## Decision

### Profiles

A profile is the ansible-core release whose behaviour Volant reproduces. Two exist: `2.19`, which is the default and stays what Volant does today, and `2.22`, added once ansible-core 2.22.0 is out and its corpus is recorded. `latest` names the newest profile Volant carries.

From 2.22 on, a profile is added for each ansible-core release, in the Volant release that ships its recordings, and `latest` moves to it. A profile is dropped when ansible-core ends support for its release. The releases between 2.19 and 2.22 get no profile of their own: their changes are part of `2.22`.

A project picks a profile with `[volant] core_version` in its `ansible.cfg` or the `VOLANT_CORE_VERSION` environment variable, which wins over the file. The value is a release (`2.19`, `2.22`) or `latest`. Any other value stops the run with exit code 5 and lists the profiles. How `volant.lock` records the profile is decided with the lock itself, in #653.

`ansible_version` reports the profile's release: 2.19.12 under `2.19`, and the patch release the `2.22` corpus was recorded with under `2.22`. This extends [decision 0003](/docs/decisions/0003-ansible-version-reports-the-reference-release/). `volant_version` does not change.

Python modules come from the ansible-core installed on the controller ([decision 0006](/docs/decisions/0006-warm-python-path/)), so their results follow that release whatever the profile. Volant warns at the start of a run when the installed release differs from the profile.

### Switches by behaviour

There is one engine. Each difference between releases is a named switch with the release it starts in, such as `loop-results-omit-skipped` from 2.21, and the code asks the profile whether the switch is on. Each switch has an entry of the same name in the registry below.

Separate behaviour sets were set aside: the measured differences are small and spread across the engine, so a copy per release would duplicate almost everything to change a key here and a message there.

Module and action-plugin code is never forked. A native module or an action plugin has one implementation, and a difference between releases is a switch inside it. A switch is tested by a scenario recorded under each release it changes, and the golden test runs every scenario under each profile against that profile's recording.

### The corpus per profile

`just golden` takes the release as an argument and records with an ansible-core of that release, installed in a `uv` environment of its own. The recordings of `2.19` stay where they are. Another profile writes `expected.<release>.json` next to `expected.json`, only for the scenarios whose result differs: 94 of 355 under 2.22.

The controller's Python is pinned with the release. The measurement found that it changes results too: with the same 2.19 templates, Python 3.12 and 3.13 answer differently to `'abcabc'.replace('b', 'X', count=1)`, to wrong arguments to string methods and to sorting a list that holds `nan`.

Recording takes about 52 minutes per release for the template, inventory and scenario goldens on a 4-core machine. It is done once by a maintainer, as today, and CI never records. CI compares Volant with the committed recordings, which costs one more run of the golden tests per profile.

A behaviour enters a profile only with a recording from that release.

### The registry

The registry lists what Ansible removed or changed, in `crates/volant/src/profile/registry.toml`. Each entry has a `name` (the switch), the `feature`, the release it was removed or changed in (`removed_in`), its `kind` (`security` or `compat`), the `replacement`, the `warning` and, for a known upstream bug, which behaviour Volant follows (`follow`, `release` or `documented`).

```toml
[[change]]
name = "debug-shows-registered-secret"
feature = "showing a registered secret with `debug`"
removed_in = "2.22"
kind = "security"
replacement = "write the value to a file with `copy` and `no_log: true`"
warning = "showing a registered secret with debug was removed in ansible-core 2.22"

[[change]]
name = "interpreter-auto-legacy"
feature = "`interpreter_python = auto_legacy` and `auto_legacy_silent`"
removed_in = "2.21"
kind = "compat"
replacement = "`auto` or `auto_silent`"
warning = "interpreter_python = auto_legacy was removed in ansible-core 2.21"

[[change]]
name = "loop-results-omit-skipped"
feature = "`skipped: false` in each loop result"
removed_in = "2.21"
kind = "compat"
replacement = "`r.skipped | default(false)`"
warning = "loop results lost their skipped key in ansible-core 2.21"
```

Under a profile at or after `removed_in`, a `compat` feature keeps working, with the warning followed by "Volant still runs it" and the replacement. A `security` feature stays off unless `[volant] allow_insecure_legacy = true`. Under an older profile the feature is simply there, without a warning.

An entry is `security` when the release notes tie the change to a CVE, when the removed behaviour can show a secret, or when it lets playbook content read or write outside what it was given. A security fix that ansible-core ships in a 2.19 patch release after 2.19.12 also applies to the `2.19` profile. The known `security` entries are:

- `debug` showing a registered secret, and `no_log` options replaced by a placeholder in results, both changed in 2.22;
- secret masking with `$REDACTED$` in all output and in `log_path`, 2.22;
- `tempfile` accepting a path in `prefix` and `suffix`, removed in 2.19.13, 2.20.9 and 2.21.4;
- credentials shown in URLs by `uri`, `get_url`, `apt_key`, `rpm_key` and the `url` lookup, masked from 2.19.13, 2.20.9 and 2.21.4;
- the ansible-connection persistent directory without private permissions, fixed in 2.19.14, 2.20.10 and 2.21.5;
- `ansible-galaxy collection install` accepting `FILES.json` entries outside the collection (CVE-2026-89091), 2.22.0rc1.

The other removals of the porting guides are `compat`: among them `DEFAULT_TRANSPORT=smart` and the `vaultid` parameter of the `vault` filters (2.20), the `paramiko` connection plugin and the `gpg_whitelist` alias of `git` (2.21), and `yes` and `no` for `follow_redirects` (2.22).

### `volant upgrade-plan`

```sh
volant upgrade-plan [PATH...] [--to PROFILE] [--json]
```

It reads the playbooks, roles, inventories and `ansible.cfg` under `PATH` (the current directory by default) and lists each use of a feature that the target profile (`latest` by default) removes or changes compared with the profile in effect. Each finding gives the `file:line`, the registry entry and the suggested change.

Some changes cannot be found by reading, such as a result read through a computed name or the text of an error. The report ends with the registry entries between the two profiles that it cannot check. For those, a project records a run under its current release and replays it under the target profile with [`volant shadow replay`](/docs/start/switch-without-risk/). The divergences are the changes the run would meet.

The exit codes are 0 when nothing was found, 1 when at least one use was found, 4 when a file cannot be read and 5 for an unknown profile. `--json` gives one object per finding: `path`, `line`, `name`, `removed_in`, `kind` and `suggestion`.

### Known regressions

Volant follows the release when a change is deliberate. It follows the documented behaviour when the release's behaviour is an open bug upstream that runs the wrong thing on a host. The answer is per case and goes in the registry's `follow` field. For the regressions known in 2.21:

- interpreter discovery reusing another host's interpreter path ([ansible/ansible#87288](https://github.com/ansible/ansible/issues/87288)): `documented`, each host gets its own discovery;
- the discovery warning naming the inventory host instead of the delegated one ([ansible/ansible#87298](https://github.com/ansible/ansible/issues/87298)): `documented`;
- discovery not cached for delegated hosts ([ansible/ansible#86517](https://github.com/ansible/ansible/issues/86517)): `documented`, since only the time differs;
- `dnf` failing when a dnf plugin prints to stdout ([ansible/ansible#87366](https://github.com/ansible/ansible/issues/87366)): `release`. The module runs from the controller's ansible-core and Volant does not patch modules. No change to the output of a successful `dnf` was found in the changelog or the issue tracker.

### The lock

`volant.lock` pins the engine that runs a project. Whether it records the profile, and what a run does when the lock and `core_version` disagree, is decided in #653.

## Consequences

- A project that sets nothing keeps the `2.19` behaviour.
- A project that moves to `latest` keeps its removed features, with a warning, except the security ones.
- Every difference costs a switch, a registry entry and a recording before it ships in a profile.
- The implementation follows in these issues, by size:
  - the profile setting, `VOLANT_CORE_VERSION`, `ansible_version` per profile and the check of the controller's ansible-core (medium);
  - recording per profile: the release argument of `just golden`, `expected.<release>.json` and the golden tests per profile (medium);
  - the registry, its warnings and `allow_insecure_legacy` (medium);
  - the switches of the `2.22` profile, from the appendix below (large, split by area once the setting lands);
  - `volant upgrade-plan` (medium).

## Appendix: corpus diff, 2026-10-25

ansible-core 2.20.10, 2.21.5 and 2.22.0rc1 against 2.19.12. The first three ran on Python 3.12, 2.22.0rc1 on Python 3.13. Left out: host order under `order: shuffle` and under `throttle`, which varies from run to run; job file paths and process ids; `ansible_version` itself; and the template differences that came from Python rather than from ansible-core.

| Feature | 2.19.12 | 2.20 | 2.21 | 2.22 | Kind |
|---|---|---|---|---|---|
| `regex` test with an unknown `match_type` | `'str' object is not callable` | `Invalid match_type specified` | as 2.20 | as 2.20 | template |
| `password_hash` with a given salt | uses the salt (`$1$saltsalt$…`) | as 2.19 | another salt (`$1$n34PoBLM$…`) | as 2.21 | template |
| Error inside a `when` | `Error while evaluating conditional: …` | as 2.19 | `A 'when' expression failed: …` | as 2.21 | template |
| A failing command's `msg` | `non-zero return code` | as 2.19 | `The command exited with a non-zero return code.` | as 2.21 | module |
| `include_vars` errors | path unquoted | path quoted | as 2.20 | as 2.20 | module |
| Check-mode result of `copy` | no `file` key | `file` key | as 2.20 | as 2.20 | module |
| `command` whose `chdir` fails | `rc: null` | as 2.19 | no `rc` key | as 2.21 | module |
| `cmd` of a command not found | a string | as 2.19 | as 2.19 | a list | module |
| `ansible_parsed` in module results | absent | absent | absent | `true` | module |
| `skipped: false` in loop results | present | present | absent | absent | scenario |
| `changed_when_result` | absent | absent | present | present | scenario |
| `failed_when` message | `Action failed: Unknown error.` or the action's name | as 2.19 | `A 'failed_when' expression evaluated to 'True'.` | as 2.21 | scenario |
| `until` retry line and `attempts` | `(1 retries left)`, `attempts: 1` | as 2.19 | `(0 retries left)`, `attempts: 2` | as 2.21 | scenario |
| Loop items reading a fact set in the same loop | `a-1`, `b-2`, `c-3` | as 2.19 | `a-0`, `b-1`, `c-2` | as 2.21 | scenario |
| `any_errors_fatal: "{{ false }}"` after a failure | the other host stops | as 2.19 | the other host goes on | as 2.21 | scenario |
| Missing file in `include_tasks` | `include` and `reason` keys | as 2.19 | `msg` and an `[ERROR]` line | as 2.21 | scenario |
| Missing `copy` source | one-line message | as 2.19 | `<<< caused by >>>` and the paths searched | as 2.21 | scenario |
| Role file outside the role | exit code 4 | exit code 4 | exit code 1 | exit code 1 | scenario |
| `debug` of an undefined `msg` | `{"msg": null}` | as 2.19 | `{}` | as 2.21 | scenario |
| `--start-at-task` with a role argument spec | validation skipped | validation runs | as 2.20 | as 2.20 | scenario |
| `ansible --help` | `-K` alone | `-K \| --become-password-file` | as 2.20 | as 2.20 | scenario |
| Secret values in output | shown | shown | shown | `$REDACTED$`, even inside a path | scenario |
| Vaulted value in `ansible-inventory --graph` | shown | shown | shown | `$REDACTED$` | inventory |

Under 2.22 the masking also hid parts of output that are not secrets: the become password `wrong` turned the directory `become-su-local-wrong-password` into `become-su-local-$REDACTED$-password`.
