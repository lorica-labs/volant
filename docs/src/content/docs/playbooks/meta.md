---
title: Meta tasks
description: What each meta action does, which host's condition counts, and how a forgiven host comes back.
---

`meta` asks the engine for something instead of running a module. Every action Ansible 2.19 has runs here, and each one prints a `TASK [meta]` banner. A meta task adds nothing to the recap: its `skipping:` line is not counted either.

| Action | What it does | Whose `when` counts |
|---|---|---|
| `flush_handlers` | Runs the handlers the host has been told about, where the task stands. | Each host's own. |
| `noop` | Nothing. | Ignored, with a warning. |
| `end_host` | The host leaves the play. No more tasks, no `always` sections, no handlers. | Each host's own. |
| `end_role` | The host skips the rest of the role it is in, an `always` section of the role included. | Each host's own. |
| `end_batch` | The batch ends for every host, and the next batch starts. | The first host's. |
| `end_play` | The play ends for every host, with whatever batches are left. | The first host's. |
| `clear_facts` | Forgets the facts modules gathered, on every host of the batch. Facts from `set_fact` stay. | The first host's. |
| `clear_host_errors` | Forgives the failures and the unreachable hosts of the batch. See below. | The first host's. |
| `refresh_inventory` | Reads every inventory source again. See below. | Ignored, with a warning. |
| `reset_connection` | Closes the host's connection, and the shared `ssh` connection with it. The next task opens both again. | Ignored, with a warning. |

"The first host" is the first host of the batch that is still in the play, in inventory order. The actions that read its `when` run once and print one banner. The others run for each host and print one banner per host that is still in the play.

`end_role` outside a role stops the run before it starts, with exit 4 and the reference's message.

## Hosts that leave

A host that ends with `end_host`, `end_batch` or `end_play` has not failed, and the recap shows it as it stood. Inside a `rescue`, the failure it rescued counts as rescued and the host stays out of the exit code. A host that had already failed keeps its failure. Later plays run on the host again.

## Forgiving errors

`clear_host_errors` takes the failures of the batch off the exit code. The recap keeps its counts, so a host can show `failed=1` in a run that exits 0.

- A host that failed does not come back to the play it failed in. It is back in the plays after it.
- A host that went unreachable comes back. It picks the play up where it stopped, and runs the steps it missed after every other host is done. If it is unreachable again, it counts again.
- A host that was unreachable in an earlier play also comes back, from the first task of the play that cleared. It is unreachable again in the plays after, and the exit code says 4 either way.

## Refreshing the inventory

`refresh_inventory` reads every inventory source again, in the order of `-i`. `groups` and `hostvars` change at once. A host the sources gained joins from the next play, and a host they lost leaves the play where it stands, without failing. Hosts and groups made by `add_host` and `group_by` stay. If the sources cannot be read any more, the inventory is empty from then on, as in Ansible: every host leaves, and the next plays match nothing.

Facts and `set_fact` values stay as they were. See [Inventories](/hosts/inventories/).
