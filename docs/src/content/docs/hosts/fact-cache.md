---
title: The fact cache
description: Keep gathered facts between runs with the jsonfile cache, and the settings that control it.
---

By default facts live in memory and are gone when the run ends. With `fact_caching = jsonfile`, Volant writes each host's facts to a file and reads them back on the next run, in the file format ansible-core 2.19 uses. A cache written by one tool is read by the other.

```ini title="ansible.cfg"
[defaults]
gathering = smart
fact_caching = jsonfile
fact_caching_connection = /var/cache/volant/facts
fact_caching_timeout = 3600
```

## Settings

| Setting | Environment | Meaning |
|---|---|---|
| `fact_caching` | `ANSIBLE_CACHE_PLUGIN` | `memory` (the default, per run) or `jsonfile`. `ansible.builtin.memory` and `ansible.builtin.jsonfile` work too. |
| `fact_caching_connection` | `ANSIBLE_CACHE_PLUGIN_CONNECTION` | The directory that holds the files. `jsonfile` needs it; without it the run keeps facts in memory. |
| `fact_caching_timeout` | `ANSIBLE_CACHE_PLUGIN_TIMEOUT` | Seconds a file stays fresh. Default 86400. `0` never expires. |
| `fact_caching_prefix` | `ANSIBLE_CACHE_PLUGIN_PREFIX` | Put in front of each file name. Empty by default. |
| `gathering` | `ANSIBLE_GATHERING` | `implicit` (default), `explicit` or `smart`. |

Each host has one file, named `<prefix>s1_<host>`.

Any other cache plugin, such as `redis` or `yaml`, ends the run with exit code 4 and the message `fact cache plugin '<name>' (from '<origin>') is not supported yet; use 'memory' or 'jsonfile'`.

## Gathering

- `implicit` gathers facts in every play that does not turn it off.
- `explicit` gathers only in plays that set `gather_facts: true`.
- `smart` gathers only for hosts that have no fresh entry in the cache.

Cached facts are loaded at the start of the run, so tasks see them even when a play does not gather.

## set_fact

`set_fact` with `cacheable: true` also writes the value to the host's cache entry, and the next run reads it back.

## Flushing

`--flush-cache` removes the entries of the hosts in the inventory before the run. Files of other hosts stay.

## Errors

A cache directory that cannot be created, or a missing `fact_caching_connection` (`Required config '_uri' for 'jsonfile' cache plugin not provided.`), warns and the run goes on with the in-memory cache. A file that is not valid JSON is removed and stops the run, with ansible-core's message: `The cache file <path> was corrupt, or did not otherwise contain valid data. It has been removed, so you can re-run your command now.`
