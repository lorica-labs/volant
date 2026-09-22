# The Python path

Volant has a Python path for Ansible modules that are still implemented by ansible-core. The controller asks a helper process to build ansible-core's payload. It sends the result to the managed host, where a warm Python server runs the module.

The managed host needs a Python interpreter but not ansible-core. The payload carries the module and every `module_utils` file it imports, so the host never has to. The controller does need ansible-core. Volant chooses its Python in this order: `VOLANT_PYTHON`, the active virtual environment, then `python3` on `PATH`.

## One payload per run

Volant merges every module needed by a run into one zip, called the union blob. It includes the modules and their imported `module_utils` files. The blob is named by the BLAKE3 hash of its contents. Volant sends it once per run, caches it on the host, and verifies the hash before using it.

The agent starts one Python server per target user. The server loads `module_utils` from the union blob once and forks a child for each task. The child runs the module and writes its JSON result to the server over a pipe. Module results are untrusted and are never rendered as controller templates.

## Measured cost

These measurements use an Ubuntu 24.04 target with Python 3.12 over SSH. Each figure is a median from the measured runs.

Ansible with its default configuration costs 765 ms per task over SSH without pipelining. With `pipelining = True`, the reference costs 469 ms per task. That is the fair baseline for Volant. Running the payload as a plain process costs 340 ms per task. Forking from a Python server that preloaded nothing costs 266 ms. Forking from a server that already loaded `module_utils` costs 12.8 ms.

Volant's measured per-task costs are 12.8 ms for `ping`, 31.3 ms for `stat`, 31.6 ms for `file`, 22.0 ms for `lineinfile`, and 755 ms for `apt`. `apt` genuinely talks to `dpkg`. The warm path removes overhead, not work. A page that implies otherwise is selling something.

The union blob for `ping`, `stat`, `file`, `lineinfile`, and `apt` is exactly 631050 bytes. Sending a separate zip for each module would send 2307982 bytes, so the union blob saves 72.7 percent. Preloading it costs 264.5 ms once per server.

## Refused modules

Volant refuses modules that ansible-core runs through an action plugin:

```
add_host assemble async_status copy dnf fetch gather_facts group_by package reboot
script service set_stats template unarchive uri wait_for_connection
```

`assert`, `fail` and `pause` have action plugins too. None of them calls a module, so Volant runs them on the controller.

The action plugin contains behavior that the module alone cannot provide. `package` chooses the host's package manager, `service` chooses its init system, and `template` renders on the controller. Sending those modules without their action plugins would do something different from what the playbook requested.

`setup` is not refused, so fact gathering works.
