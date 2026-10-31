---
sourceHash: b638cbf2bfa7
title: Nombres de comandos de ansible
description: Qué comando de Volant sustituye a cada programa de ansible-core, qué es igual, qué difiere y cómo conservar tus líneas de comandos.
---

Volant ejecuta los nueve programas de ansible-core 2.19.12. Cada uno es un subcomando de `volant`, y en Linux y macOS también se puede iniciar con su nombre de ansible. Windows no se admite ni para los nombres de programa ni para los enlaces.

## Qué sustituye a qué

| Programa de ansible-core | Comando de Volant | Se ejecuta en | Runtime administrado |
|---|---|---|---|
| `ansible` | [`volant adhoc`](/es/docs/reference/cli/adhoc/) | Volant | no hace falta para los módulos nativos |
| `ansible-playbook` | [`volant playbook`](/es/docs/reference/cli/), `volant-playbook` | Volant | se instala en el primer uso que necesita Python |
| `ansible-vault` | [`volant vault`](/es/docs/reference/cli/vault/) | Volant | no hace falta |
| `ansible-inventory` | [`volant inventory`](/es/docs/reference/cli/inventory/) | Volant | no hace falta, salvo para los plugins de inventario de las colecciones |
| `ansible-doc` | [`volant doc`](/es/docs/reference/cli/doc/) | la herramienta propia de ansible-core | se instala en el primer uso |
| `ansible-config` | [`volant config`](/es/docs/reference/cli/config/) | la herramienta propia de ansible-core | se instala en el primer uso |
| `ansible-galaxy` | [`volant galaxy`](/es/docs/reference/cli/galaxy/) | Volant, en desarrollo: los subcomandos se rechazan hasta que sean nativos | no hace falta |
| `ansible-pull` | [`volant pull`](/es/docs/reference/cli/pull/) | Volant | no hace falta para un checkout `git` |
| `ansible-console` | [`volant console`](/es/docs/reference/cli/console/) | Volant | no hace falta para los módulos nativos |

El runtime administrado se instala solo la primera vez que un comando lo necesita, como con un playbook, e imprime una línea en el error estándar cuando lo hace. [El runtime administrado](/es/docs/reference/runtime/) lo describe. No necesitas instalar ansible, pero dos de los nueve comandos ejecutan el código propio de ansible-core desde ese runtime.

## Conservar tus líneas de comandos

Los enlaces no se crean si no lo pides. Ejecuta el script de instalación con `--shims`, o ejecuta [`volant shims install`](/es/docs/reference/cli/shims/):

```sh
curl -fsSL https://volant.sh/install.sh | sh -s -- --shims
volant shims install
```

Cada nombre es un enlace simbólico a `volant`. Volant lee el nombre de archivo con el que se inició, así que el enlace `ansible-vault` ejecuta `volant vault`. Ambos comandos toman los mismos argumentos y dan la misma salida y los mismos códigos de salida. Un error de uso sale con 2 y nombra el programa tal como se inició, `ansible-vault: error: ...`. Como ansible-core, un enlace comprueba la configuración regional antes de leer sus argumentos. Una que no puede establecer, o que no es UTF-8, sale con 1 y `ERROR: Ansible could not initialize the preferred locale` o `ERROR: Ansible requires the locale encoding to be UTF-8`. `LC_ALL=C` se rechaza; sin ninguna variable de configuración regional, la configuración C cuenta como C.UTF-8. `volant` en sí no comprueba nada.

`install` no reemplaza un archivo o enlace que no creó, salvo con `--force`. Cuando hay otro programa con ese nombre en tu `PATH`, lo dice: `note: /usr/bin/ansible-vault is hidden by this link, DIR comes first in PATH` cuando gana el enlace, o `note: ... still runs first, DIR comes later in PATH` cuando no. Para eliminar los enlaces:

```sh
volant shims remove
```

`remove` elimina solo los enlaces que apuntan a un archivo llamado `volant`, y deja todo lo demás.

Un script de CI como este se ejecuta sin cambios una vez que existen los enlaces, sin ansible en la máquina:

```sh
ansible-inventory -i hosts.ini --list > inventory.json
ansible-vault view group_vars/all/secrets.yml --vault-password-file ci-pass
ansible-playbook -i hosts.ini site.yml
```

## Qué es igual

Para `ansible`, `ansible-vault` y `ansible-inventory`, la salida, las opciones listadas en las páginas de los comandos y los códigos de salida se midieron frente a ansible-core 2.19.12. Cada afirmación tiene un test detrás.

| Afirmación | Dónde se comprueba |
|---|---|
| `volant adhoc` imprime los bytes del callback `minimal`, el bloque `[ERROR]` y las formas `-o` y `-t`, con los códigos de salida de la referencia | los goldens de escenario `adhoc-*` y `crates/volant/tests/adhoc_cli.rs` |
| El nombre `ansible` ejecuta `volant adhoc` | `the_ansible_name_runs_adhoc` |
| El nombre `ansible-playbook` ejecuta `volant playbook` | `a_link_named_ansible_playbook_behaves_as_the_playbook_subcommand` |
| `volant vault` da los mensajes, códigos de salida, modos de archivo y archivos de ansible-vault | `crates/volant/tests/vault_cli.rs`, y los goldens de escenario `vault-*` para los datos de vault que lee un playbook |
| Un archivo que cifra una herramienta lo descifra la otra, con el id por defecto y con una etiqueta | `a_file_one_tool_encrypts_the_other_decrypts` |
| `volant inventory` imprime los bytes JSON y YAML de ansible-inventory: orden de claves, sangría de cuatro espacios, anclas de PyYAML | `crates/volant/tests/inventory_golden.rs` sobre `golden/inventories/cases.yml`, e `inventory_cli.rs` |
| `volant config` imprime lo que imprime la herramienta | `crates/volant/tests/tools_cli.rs` |
| `volant galaxy` acepta las opciones de ansible-galaxy y hace sus primeras comprobaciones con los mismos mensajes | `crates/volant/src/galaxy/tests.rs`, y `galaxy_golden.rs` contra un servidor Galaxy falso |
| `volant pull` imprime la ejecución del playbook y la línea de `-o` como `ansible-pull`, y hace el checkout sin Python | `a_pull_prints_what_the_reference_pull_prints` y `a_pull_checks_out_and_runs_without_python` en `console_pull.rs` |
| `volant console` imprime en una tubería lo que imprime `ansible-console` para sus comandos, módulos y líneas de shell, y no necesita Python para los módulos nativos | `the_console_reads_its_commands_from_a_pipe` y `the_console_runs_without_python` en `console_pull.rs` |
| Los enlaces que crea `install`, las líneas que imprime y lo que `remove` deja | `crates/volant/tests/shims.rs` |
| Qué nombres corresponden a qué comando, y ningún otro | los tests de `crates/volant/src/cli/program.rs` |

Las opciones que acepta cada comando nativo están listadas en su propia página: [`volant adhoc`](/es/docs/reference/cli/adhoc/), [`volant vault`](/es/docs/reference/cli/vault/), [`volant inventory`](/es/docs/reference/cli/inventory/) y [`volant pull`](/es/docs/reference/cli/pull/). Una opción que no figura allí se rechaza, nunca se ignora.

## Qué difiere

- El texto de ayuda es el de Volant. Nunca se compara con la referencia.
- `--version` imprime `ansible [core 2.19.12]` (o el nombre del programa) y las rutas. Las líneas de Python, Jinja y PyYAML se omiten cuando Volant no encuentra ningún intérprete con ansible-core, porque `--version` no instala nada. Consulta [`volant adhoc`](/es/docs/reference/cli/adhoc/).
- `executable location` en `--version` es el programa tal como se inició: `volant`, o el enlace. Consulta [`volant doc`](/es/docs/reference/cli/doc/#terminal-código-de-salida-y---version).
- `ansible-inventory --toml` no necesita ningún paquete de Python, donde la referencia necesita `tomli-w`, y un valor `null` da una línea `[ERROR]` en lugar de un traceback (ambos salen con 250). Consulta [`volant inventory`](/es/docs/reference/cli/inventory/).
- `volant pull` imprime sus dos primeras líneas antes de lo que imprimen sus pasos, y su checkout no inicia Python. Consulta [`volant pull`](/es/docs/reference/cli/pull/#diferencias-con-ansible-pull).
- `volant config dump` muestra `COLLECTIONS_PATHS` como fijado por el entorno una vez que el runtime tiene un directorio `collections/`. Consulta [`volant config`](/es/docs/reference/cli/config/#la-ruta-de-colecciones).
- `ansible-vault view` pasa por un paginador solo en un terminal, con `PAGER`, luego `ANSIBLE_PAGER`, luego `[defaults] pager`, luego `less`. Consulta [Vault](/es/docs/variables/vault/).
- `volant adhoc` y `volant playbook` usan el motor de Volant, así que se aplican las diferencias listadas en [¿Se ejecutará mi playbook?](/es/docs/start/compatibility/) y en [`volant adhoc`](/es/docs/reference/cli/adhoc/#acciones-que-no-son-módulos).

## Códigos de salida

| Comando | 0 | 1 | 2 | 4 | 5 | 250 |
|---|---|---|---|---|---|---|
| `volant adhoc` | todos los hosts tuvieron éxito o se omitieron | no queda ningún host al que apuntar, o una acción `meta` incorrecta | un host falló, o un error de uso | un host era inalcanzable, o algo que esta versión todavía no admite | un ajuste no válido, o ningún argumento para `command` | un argumento que no es JSON válido |
| `volant vault` | hecho | un error de archivo, como `create` sobre un archivo existente | un error de uso | | una opción incorrecta, como dos archivos con `--output` | |
| `volant inventory` | impreso | | un error de uso | | ninguna acción o dos acciones, o un destino incorrecto de `--host` o `--graph` | |

`volant playbook` usa los códigos de [Códigos de salida](/es/docs/reference/exit-codes/). Las tres herramientas que se ejecutan en el runtime de ansible-core devuelven el código propio de la herramienta, o 1 con una línea `[ERROR]:` cuando no se puede iniciar el intérprete. `volant pull` sale con 2 cuando el checkout falla, con 5 ante un error de opciones o cuando no encuentra ningún playbook, y si no con el código del playbook.
