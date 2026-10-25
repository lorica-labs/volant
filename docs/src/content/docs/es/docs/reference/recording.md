---
sourceHash: 0c8e4bf84fac
title: Grabaciones de ejecuciones
description: El archivo que escribe el callback volant.compat.record mientras se ejecuta ansible-playbook, línea por línea.
---

Antes de cambiar de herramienta, un equipo puede grabar lo que hace `ansible-playbook` en sus ejecuciones reales y hacer que Volant las reproduzca más tarde. La grabación la escribe el callback `volant.compat.record`. Se distribuye en la colección `volant.compat`, solo necesita la biblioteca estándar de Python y funciona con ansible-core 2.19 y versiones posteriores.

## Activarlo

Instala la colección donde ansible-core encuentra sus colecciones y añade dos secciones a `ansible.cfg`:

```ini
[defaults]
callbacks_enabled = volant.compat.record

[callback_volant_record]
path = shadow/run-{{ timestamp }}.jsonl.gz
```

Es un callback de notificación. Nunca sustituye al callback de salida estándar, y la salida y el código de salida de la ejecución son los mismos que sin él.

| Ajuste | Significado |
|---|---|
| `path` | El archivo que se escribe. `{{ timestamp }}` se convierte en la hora de inicio de la ejecución, en UTC, con la forma `20261025T093000Z`; no se lee ninguna otra plantilla. Un nombre que termina en `.gz` se comprime con gzip. Los directorios padre que faltan se crean con el modo `0700`. El valor por defecto es `shadow/run-{{ timestamp }}.jsonl.gz`. |

`VOLANT_RECORD_PATH` define `path` desde el entorno, lo que encaja en un job de CI.

Cuando no se puede escribir el archivo, el callback muestra un aviso y la ejecución continúa sin grabación.

## Seguridad

Una grabación guarda los resultados de las tareas tal como los vio ansible-core. Puede contener valores de vault descifrados, las variables extra de la ejecución y todo lo que devolvió un módulo. Trátala como un secreto:

- El archivo se crea con el modo `0600`, y un archivo existente vuelve a `0600`.
- Mantén el directorio de grabaciones fuera del control de versiones y guarda las grabaciones de CI como artefactos privados con una retención corta.
- Una tarea con `no_log` conserva su estado, y su resultado se graba como `{"censored": true}`.

## Formato

Una grabación es JSON Lines: un objeto JSON por línea, cada uno con una clave `event`. Esta página describe el formato `1`.

| `event` | Campos |
|---|---|
| `header` | `format`, `ansible_core` (su versión), `cli` (las opciones de línea de comandos de la ejecución, tal como las leyó ansible-core), `cwd`, `playbooks` (el `path` y el `sha256` de cada playbook), `inventory` (las `sources` tal como se dieron, y el `path` y el `sha256` de cada fuente que es un archivo), `extra_vars_hash`, `started` |
| `inventory` | `hosts`: cada host del inventario, con los grupos a los que pertenece además de `all` |
| `play` | `uuid`, `name`, `hosts` (el patrón del play) |
| `task` | `uuid`, `path` (`archivo:línea`), `name`, `action` (el nombre completo del módulo o la acción), `handler`, `role` |
| `result` | `host`, `task` (su `uuid`), `item`, `status`, `delegated_to`, `result` |
| `retry` | `host`, `task`, `attempt`, `result`: un intento fallido de un bucle `until` |
| `include` | `file`, `hosts`: un archivo que añadió `include_tasks` o `include_role`, y los hosts que lo ejecutan |
| `notify` | `handler`, `host`: un handler que se ejecutará en un host |
| `stats` | `hosts` (el resumen de cada host), `custom` (lo que reunió `set_stats`) |

Las líneas `header` e `inventory` van primero, al empezar el primer play. `stats` es la última línea de una ejecución completa, así que una grabación sin ella viene de una ejecución interrumpida.

Notas sobre los campos:

- Cada hash es SHA-256 en hexadecimal. `extra_vars_hash` cubre cada argumento `-e` tal como se dio, seguido de un byte NUL, y para un argumento `@archivo` el contenido del archivo, seguido de otro byte NUL. Una reproducción pasa los mismos argumentos, así que el hash solo cambia cuando cambia un archivo que nombran.
- `status` es `ok`, `changed`, `failed`, `skipped` o `unreachable`. Un fallo que `ignore_errors` dejó pasar sigue siendo `failed`.
- En un bucle, cada elemento tiene una línea `result` cuyo `item` es su índice desde 0. La línea siguiente, con `item` a `null`, es la tarea entera, con los elementos bajo `results`.
- `delegated_to` es el host que nombró un `delegate_to`, o `null`.
- `result` es el resultado de la tarea tal como lo reciben los callbacks, incluidas las claves que añade ansible-core, como `invocation`, `false_condition`, `skip_reason` y `attempts`. Qué claves aparecen depende de la versión: ansible-core 2.21 ya no pasa `invocation` a los callbacks.
- El resultado de la recogida de facts se graba entero, porque una reproducción renderiza plantillas con él. Son de 50 a 100 KB por host, por eso gzip es el valor por defecto.
- `include_tasks` produce una línea `task` y un `result` por host propios, y luego una línea `include` por archivo. `import_tasks` no produce ninguno de los dos, solo las tareas importadas.
- Las líneas `notify` se escriben cuando se ejecutan los handlers, después de las tareas que los notificaron.
