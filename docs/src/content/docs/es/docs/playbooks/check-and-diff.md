---
sourceHash: 4a0054ff06cb
title: Modo check, diff y listados
description: Qué hacen --check y --diff, qué módulos los atienden, cómo seleccionan tareas las etiquetas, y los comandos que describen una ejecución sin iniciarla.
---

Esta página responde a cómo previsualizar una ejecución antes de que cambie nada: simulaciones con `--check`, cambios en archivos con `--diff`, selección de tareas con etiquetas y listados que no se conectan a nada.

`--check` informa de lo que cambiaría cada tarea sin cambiarlo. `--diff` imprime los cambios que un módulo hace en los archivos. Ambos funcionan como en `ansible-playbook` 2.19.12, y el recap y el código de salida coinciden.

Las palabras clave de tarea ganan a las opciones. `check_mode: false` ejecuta la tarea de verdad bajo `--check`. `check_mode: true` convierte una tarea en una simulación dentro de un play normal. `diff: false` mantiene fuera de la pantalla los cambios de una tarea bajo `--diff`.

`ansible_check_mode` y `ansible_diff_mode` informan solo de `--check` y `--diff` de la línea de comandos. La palabra clave `check_mode` o `diff` de un play o de una tarea no los cambia.

## Qué se ejecuta dónde

Una tarea en modo check nunca cambia un host, sea cual sea la ruta que la ejecute.

Volant lleva una versión nativa de algunos módulos, escrita en Rust y ejecutada dentro del agente, el programa pequeño que sube a cada host. Cuando la versión nativa no puede atender una tarea, la devuelve: en su lugar se ejecuta en el host el propio código de Python del módulo.

| Tarea | Qué ocurre |
|---|---|
| `command`, `shell`, `raw` | Se omiten, como en ansible-core. Un `command` o `shell` con `creates` o `removes`, en la línea o en `args:`, se comprueba: `ok` cuando la guarda lo detiene, `changed` cuando se ejecutaría. |
| `stat`, `setup`, `package_facts`, `service_facts` | Los atiende el agente. Solo leen, así que check y diff no cambian nada. |
| `file`, `lineinfile`, `systemd`, `user`, `group`, `apt` | Se devuelven al código de Python del módulo en el host, que sabe qué habría cambiado y cómo expresar el diff. |
| `copy`, `template`, `fetch`, `unarchive` y otros plugins de acción | Los ejecuta el propio plugin de ansible-core mediante el helper de Python del controlador (un proceso que ejecuta ansible-core en el controlador), que calcula por sí mismo el resultado del check y el diff. |
| Módulos del controlador (`debug`, `set_fact`, `assert`, ...) | Se ejecutan como siempre. |
| `package`, `service`, `dnf` | Eligen el módulo para el host, y después ese módulo se ejecuta en modo check. |

El perfil (`--profile`) nombra el motivo de cada devolución: "the task runs in check mode" o "the task shows a diff".

## Un host sin Python

Devolver la tarea necesita Python en el host. Sin él, todos los módulos nativos que cambian el host fallan bajo `--check` o `--diff`: `apt`, `user`, `group`, `file`, `lineinfile`, `systemd`, `systemd_service` y el módulo `copy` que ejecuta el plugin de copia. Instala Python en el host, o ejecuta el play sin esas opciones. Las tareas que solo leen (`stat`, `setup`, `package_facts` y `service_facts`), y `command`, `shell` y `raw`, siguen funcionando.

## Salida de diff

Un diff se imprime solo para una tarea que cambió algo. `copy` y `template` no imprimen nada cuando `diff` está desactivado. El proceso helper solo se inicia cuando se imprime un diff.

## Carencias conocidas

- `-vvvv` enumera las opciones que enumera ansible-core, en su orden, y las líneas de preparación de plugins. Sus líneas de ssh muestran los comandos que este motor ejecuta por `ssh` (la sonda, la subida y el enlace con el agente), no la subida por `sftp` de ansible-core, y una conexión local o un archivo de módulo no imprimen ninguna línea.

La [página de paridad](/es/docs/reference/parity/) enumera cada opción y palabra clave con su estado.

## Etiquetas

`tags` se puede escribir en un play, en la entrada de un rol, en un bloque o en una tarea. El compilador pliega las etiquetas externas en cada tarea que tienen debajo. `--tags` y `--skip-tags` deciden después qué tareas se compilan, así que las etiquetas deciden tanto qué se ejecuta como qué muestran los listados.

### Las reglas de las etiquetas

Volant aplica las reglas de Ansible tal cual:

| Etiqueta en la tarea | Comportamiento |
|---|---|
| `always` | La selecciona cualquier `--tags`. Sobrevive a `--skip-tags all` salvo que se omita el propio `always`. |
| `never` | Queda fuera de `--tags all` y `--tags tagged`. Solo vuelve cuando una selección la nombra. |
| sin etiqueta | Se trata como la etiqueta `untagged`, por eso `--tags untagged` la selecciona. |

`[tags] run` y `[tags] skip` en `ansible.cfg`, y `ANSIBLE_RUN_TAGS` y `ANSIBLE_SKIP_TAGS`, son valores por defecto. La línea de comandos se añade a ellos en lugar de reemplazarlos.

Las etiquetas escritas en un `include_tasks` o un `include_role` se quedan en la sentencia: `--tags inc` ejecuta el include, no lo que trae. Las etiquetas del play, de la entrada del rol y de los bloques que rodean la sentencia sí llegan a las tareas incluidas, así que `--tags nginx` en un rol etiquetado `nginx` ejecuta los archivos que ese rol incluye. Ansible se comporta igual.

### `ansible_run_tags` y `ansible_skip_tags`

Una tarea puede leer la selección de etiquetas de su propia ejecución como dos variables: `ansible_run_tags` contiene `--tags` (`["all"]` cuando no se dio) y `ansible_skip_tags` contiene `--skip-tags` (`[]` cuando no se dio), cada una después de incorporar `[tags] run` / `[tags] skip` de `ansible.cfg` y las dos variables de entorno `ANSIBLE_RUN_TAGS` / `ANSIBLE_SKIP_TAGS`.

```yaml
- name: Only fetch the kubeconfig once it has been asked for
  fetch:
    src: /etc/rancher/k3s/k3s.yaml
    dest: "{{ kubeconfig }}"
  when: "'kubeconfig' in ansible_run_tags"
```

## Mira antes de ejecutar

Cuatro opciones leen un playbook e imprimen lo que haría una ejecución, sin conectarse a nada:

```sh
volant playbook -i inventory.ini site.yml --list-hosts
volant playbook -i inventory.ini site.yml --list-tasks
volant playbook -i inventory.ini site.yml --list-tags
volant playbook -i inventory.ini site.yml --syntax-check
```

Su salida coincide con la de Ansible hasta el último espacio en blanco. La suite de pruebas la compara byte a byte con ansible-core sobre un corpus de playbooks.

Los listados recorren solo el cuerpo de un bloque, así que una tarea de `rescue:` o `always:` no aparece, igual que en Ansible.

## Diferencias con Ansible

- Un play con varias etiquetas las imprime ordenadas, y `--list-hosts` imprime los hosts de un play en el orden del inventario. Ansible imprime un conjunto de Python en ambos sitios, así que su orden cambia de una ejecución a otra.
- Un listado no resuelve módulos. Un playbook cuyos módulos esta versión no puede ejecutar se lista igualmente y pasa `--syntax-check`, así que puedes leer un rol antes de la versión que lo ejecute.
- Una tarea cuyo `ignore_errors` o `become` no es ni verdadero ni falso hace que un listado termine con el código 4. Ansible la lista y termina con 0. El valor es un playbook mal formado, no una función sin implementar.
