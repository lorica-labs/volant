---
sourceHash: 30b19ae888bc
title: Bucles y reintentos
description: loop, with_items y loop_control, y cómo until, retries y delay repiten una tarea.
---

Esta página responde a cómo repetir una tarea sobre una lista de elementos con `loop` y `with_<lookup>`, y cómo volver a ejecutar una tarea hasta que tenga éxito con `until`, `retries` y `delay`.

## Bucles

```yaml
- name: Create the service accounts
  command: useradd --system {{ item }}
  loop:
    - app
    - worker
  loop_control:
    label: "{{ item }}"
  register: accounts
```

`loop` y `with_items` no pueden aparecer a la vez en la misma tarea. `with_items` aplana un nivel de listas anidadas y `loop` no, como en Ansible. Registrar una tarea con bucle recoge una lista `results` con una entrada por elemento.

`loop_control` lee `loop_var`, `label`, `index_var`, `pause`, `extended`, `extended_allitems` y `break_when`. Una subclave que ansible-core no conoce se rechaza al cargar el playbook. Funcionan todas las formas `with_<lookup>`. Consulta [Palabras clave](/es/docs/reference/keywords/#under-loop_control) y [Bucles `with_`](#bucles-with_).

## Bucles `with_`

`with_<name>` es `loop: "{{ lookup('<name>', *terms, wantlist=True) }}"`, y funciona con todos los plugins de lookup que tiene ansible-core:

| Lookup | Ejemplos |
|---|---|
| Los veinticinco de `ansible.builtin` | `with_dict`, `with_nested`, `with_subelements`, `with_sequence` |
| Lookups que ansible-core redirige a `community.general` | `with_flattened`, `with_cartesian`, `with_filetree` |
| El lookup de una colección, con su nombre completo | `with_community.general.dependent` |
| Un plugin en un directorio `lookup_plugins/` junto al playbook o a un rol | el nombre de archivo del plugin, como `with_<name>` |

Un lookup integrado se acepta también como `with_ansible.builtin.<name>` y `with_ansible.legacy.<name>`.

El ansible-core del controlador dice si un nombre es un lookup, antes de la primera conexión. Un nombre `with_` que no lo es hace fallar el playbook con `'with_xyz' is not a valid attribute for a Task`, como en Ansible. Una tarea con dos bucles falla con `duplicate loop in task: <name>`, y una clave `with_` sin valor con `you must specify a value when using with_<name>`.

Un lookup que lanza una excepción hace fallar la tarea con la frase propia del plugin. Un lookup que Volant ejecuta por sí mismo, en Rust (`file`, `unvault`, `vars`, `pipe`, `template` y `first_found`), falla con `The lookup plugin '<name>' failed: <reason>` y la cadena de ansible-core, también en un `when:` y en un archivo de plantilla. `vars` lo expresa como `No variable named '<name>' was found.`, sin la parte del plugin. Un valor de bucle que es una sola plantilla que lee una variable sin definir falla con `'name' is undefined`. Una lista cuyo elemento lee una variable sin definir falla con la frase del motor para ello, sin la cadena que imprime Ansible.

## Renderizado de cada elemento

Un bucle renderiza los argumentos, el `when`, las palabras clave y la etiqueta de un elemento cuando llega a él, no antes de que empiece el bucle.

- Un elemento que no se renderiza falla solo, como `failed: [h] (item=2)`. Los elementos siguientes se ejecutan igualmente, y la tarea termina con `One or more items failed`.
- Un `break_when` que termina antes el bucle oculta los errores de renderizado de los elementos posteriores.
- Un `loop_control.label` que lee una variable sin definir hace fallar su elemento después de que el módulo se ejecutó.

## `with_first_found` y `with_fileglob`

```yaml
- name: Distribute the matching config
  copy:
    src: "{{ item }}"
    dest: /etc/app/config.yml
  with_first_found:
    - "{{ ansible_distribution }}.yml"
    - default.yml

- name: Copy every image
  copy:
    src: "{{ item }}"
    dest: /var/lib/app/images/
  with_fileglob:
    - "images/*.tar.gz"
```

Ambos recorren un lookup, como cualquier forma `with_<name>`: `with_first_found` recorre sus términos hasta que uno nombra un archivo que existe, y `with_fileglob` reúne todas las coincidencias de un patrón glob. `with_first_found` busca además en un subdirectorio de cada entrada de búsqueda antes que en la propia entrada: `templates/` cuando la acción de la tarea nombra `template`, `vars/` cuando nombra `include_vars` y `files/` en los demás casos, el mismo directorio en el que buscaría por sí sola una tarea `template:` o `include_vars:` normal.

Si no encuentra nada, la tarea falla con la frase propia de ansible-core, `No file was found when using first_found.`, salvo que el término sea un mapa con `skip: true`, en cuyo caso el bucle se ejecuta vacío. Un término que se renderiza como sin definir se descarta en lugar de hacer fallar todo el lookup, lo que permite que un rol lea un fact que ningún host ha definido todavía y aun así llegue a su `when`. Una ruta que encuentra cualquiera de los dos lookups y que aún contiene una marca de plantilla (`{{`, `{%` o `{#`) se rechaza por su nombre en lugar de renderizarse una segunda vez.

Una ruta que un lookup encontró en el controlador es contenido propio del playbook; un elemento construido en parte a partir de un valor del host, como un término que lee `ansible_distribution`, se trata como datos, igual que un elemento de un `loop:` sobre un valor registrado.

## Reintentos con until

```yaml
- name: Wait for the service to answer
  command: curl -fsS http://localhost:8080/health
  register: health
  until: health.rc == 0
  retries: 10
  delay: 3
```

`until` vuelve a ejecutar una tarea hasta que su expresión se cumple.

- `retries` es el número de reintentos tras el primer intento, no el total: la tarea se ejecuta hasta `retries + 1` veces. Vale 3 cuando solo se escribe `until`, con hasta cuatro ejecuciones.
- Un `retries` menor que 1 desactiva los reintentos: la tarea se ejecuta una vez y su resultado no tiene `attempts`.
- Volant espera `delay` segundos, 5 por defecto, después de cada una de las primeras `retries` ejecuciones fallidas, y nunca después de la última.
- El resultado lleva el número de reintentos realmente ejecutados en `attempts`, como máximo `retries`. Una tarea que agota los reintentos ha fallado, aunque el propio módulo tuviera éxito. Con un `failed_when`, decide en su lugar su resultado en la última ejecución: `failed_when: false` deja pasar la tarea, y el resultado registrado sigue diciendo `failed: true`.
- `retries` sin `until` repite la tarea mientras su resultado sea fallido.
- Una tarea respaldada por un plugin de acción (`copy`, `dnf`, `package`...) reintenta toda su secuencia de subtareas desde el principio en cada intento, con un plugin nuevo y cualquier archivo enviado de nuevo; `until` lee solo el resultado de la última subtarea.

:::note
Tres reintentos con el retardo por defecto gastan quince segundos de espera entre las cuatro ejecuciones, como en Ansible.
:::

Una tarea con bucle reintenta cada elemento por separado y termina los intentos de un elemento antes de empezar con el siguiente.

`until`, `retries` y `delay` se rechazan en `include_tasks` e `include_role`, antes de la primera conexión. Ansible también los rechaza ahí, al cargar el playbook. `include_vars` es un módulo normal y se reintenta como cualquier otra tarea.

## Diferencias con Ansible

- Una expresión `until` que no se puede evaluar se informa con el prefijo de ansible-core y el mensaje propio de Volant, igual que un `when` que falla.
- `FAILED - RETRYING` para una tarea dentro de un rol muestra el nombre propio de la tarea en lugar de `role : name`.
- Un `loop_control.label` que falla por un motivo distinto de una variable sin definir, como un lookup que lanza una excepción, hace fallar la tarea antes de que se ejecute ningún elemento. ansible-core termina la tarea después de que ese elemento se haya ejecutado.
