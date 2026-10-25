# volant.compat

Plugins for ansible-core that help a team move to [Volant](https://volant.sh). They need only the Python standard library and run on ansible-core 2.19 and later.

## `volant.compat.record`

A notification callback that writes what `ansible-playbook` did to a JSON Lines file, so Volant can later replay the run and say whether it would have done the same. It never replaces the stdout callback and never changes a run's result.

Enable it in `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

`{{ timestamp }}` becomes the time the run started, in UTC (`20261025T093000Z`). A name ending in `.gz` is compressed. `VOLANT_RECORD_PATH` sets the path from the environment.

The recording holds task results as ansible-core saw them, so it can contain decrypted vault values and the run's extra variables. The file is created with mode `0600`. Keep it out of version control. A task with `no_log` keeps its status, and its result is recorded as `{"censored": true}`.

The format is described in the [recording reference](https://volant.sh/docs/reference/recording/).
