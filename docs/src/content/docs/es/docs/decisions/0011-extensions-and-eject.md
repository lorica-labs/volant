---
sourceHash: 528668a2f068
title: "Extender Ansible solo con lo que puede volver a él"
description: "Registro de decisión 0011."
sidebar:
  label: "0011 · Extensiones y eject"
  order: 11
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-10-25 |

## Contexto

Volant ejecuta los playbooks de Ansible como lo hace ansible-core 2.19 ([decisión 0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/)). Una vez logrado eso, puede ofrecer lo que le falta a Ansible. El riesgo es un proyecto que acaba dependiendo de una de esas funciones y ya no puede volver a ansible-core. Un equipo adopta Volant con más facilidad cuando sabe que puede irse en cualquier momento.

Dos cosas hacen posible la vuelta. El YAML sigue siendo la fuente: nada lo reemplaza. Y cada extensión tiene una regla que la reescribe en Ansible simple con el mismo comportamiento, aplicada por un comando.

## Decisión

### Dos tipos de extensión

El contenido que Ansible puede cargar, como filtros, lookups y módulos, va en la colección `volant.compat`. Funciona tal cual bajo ansible-core, así que nunca necesita expulsarse.

Las palabras clave y la sintaxis que solo entiende Volant se escriben `volant.<palabra>`, como clave del play, el bloque o la tarea a los que se aplican:

```yaml
- name: one account per user
  volant.loop: "{{ users }}"
  block:
    - ansible.builtin.user:
        name: "{{ item }}"
    - ansible.posix.authorized_key:
        user: "{{ item }}"
        key: "{{ lookup('file', 'keys/' ~ item ~ '.pub') }}"
```

Una clave en lugar de un mapping `volant:`, porque así cada extensión es una clave que se puede buscar y que se reescribe por sí sola. ansible-core rechaza esa clave con claridad, medido en 2.19.12: `[ERROR]: 'volant.loop' is not a valid attribute for a Block`, con la línea. Un playbook que usa una extensión nunca se ejecuta a medio entender bajo ansible-core.

### El contrato de reescritura

Cada extensión viene con una regla de reescritura a Ansible simple. El comportamiento de la extensión se define como el de su reescritura bajo ansible-core 2.19.12: Volant no inventa semántica, ejecuta más rápido la forma reescrita y mantiene legible el original.

Cada extensión viene también con una prueba de ida y vuelta. La prueba ejecuta la forma reescrita bajo ansible-core 2.19.12 y la extensión bajo Volant, y compara la salida y el resumen. Ninguna extensión se fusiona sin ambas.

### `volant eject`

```sh
volant eject [PATH...] [--out DIR] [--json]
```

Lee los playbooks y roles bajo `PATH` (el directorio actual por defecto) y escribe cada archivo que usa una extensión con todas sus extensiones reescritas. Con `--out DIR` el árbol se escribe allí; sin él, los archivos se reescriben en su sitio. La reescritura puede añadir archivos, como el archivo de tareas incluido de un bucle reescrito. Van bajo `_volant/` junto al archivo del que vienen, con el nombre de la tarea y un hash de su contenido, de modo que expulsar dos veces da el mismo árbol.

Una extensión cuya reescritura no es exacta en un contexto dado, como `run_once` bajo la estrategia `free` más abajo, se rechaza con su `archivo:línea` y el motivo, y no se escribe nada. El informe lista cada archivo reescrito y cada archivo añadido. `--json` da un objeto por archivo: `path`, `action` (`rewritten` o `added`) y las extensiones reescritas. Los códigos de salida son 0 cuando todo se reescribió, 1 cuando se rechazó una extensión y 4 cuando no se puede leer un archivo.

### `volant check --portable`

```sh
volant check --portable [PATH...] [--json]
```

Lista cada línea que ansible-core rechazaría, con su `archivo:línea` y la extensión que usa. Sale con 0 cuando no hay ninguna y con 1 en otro caso. `--json` da un objeto por línea: `path`, `line`, `extension`. Un job de CI puede usarlo como barrera, y la salida de `volant eject` debe pasarlo.

### Las primeras extensiones

Se eligen según los votos públicos del gestor de issues de ansible-core.

1. **Un bucle sobre un bloque** (ansible/ansible#13262), `volant.loop` con `volant.loop_control` en un bloque. La reescritura mueve las tareas del bloque a un archivo incluido y hace el bucle sobre la inclusión:

   ```yaml
   - name: one account per user
     ansible.builtin.include_tasks: _volant/one-account-per-user-3f2a.yml
     loop: "{{ users }}"
   ```

   Medido en 2.19.12, con dos hosts, bajo `linear` y bajo `serial: 1`: las inclusiones se expanden primero y luego las tareas de cada elemento se ejecutan por turno, cada tarea en todos los hosts. Un `when` dentro se aplica por elemento, y `run_once` se ejecuta una vez por elemento. Bajo la estrategia `free`, ansible-core no respeta `run_once` en el archivo incluido: avisa y ejecuta la tarea en cada host. Un `run_once` dentro de un `volant.loop` bajo `free` lo rechaza por tanto `eject` y lo señala `check --portable`, ya que Volant sigue la reescritura.

2. **Un bloque como handler** (ansible/ansible#14270): un bloque en `handlers:` que una tarea puede notificar por el nombre del bloque. Medido en 2.19.12, notificar un bloque por su nombre falla con `[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list`. La reescritura convierte el bloque en un handler que incluye sus tareas:

   ```yaml
   handlers:
     - name: restart app
       ansible.builtin.include_tasks: _volant/restart-app-91c0.yml
   ```

   Medido en 2.19.12: el handler notificado incluye el archivo para cada host notificado y ejecuta sus tareas en orden, como lo haría un bloque. No necesita ninguna clave nueva, porque Volant acepta el bloque donde ansible-core no puede usarlo, así que se escribe como Ansible simple y se reescribe igualmente.

3. **`collection uninstall`** (ansible/ansible#67759), un comando de `volant galaxy` y no una sintaxis. No cambia nada en un playbook, así que no necesita reescritura y nunca bloquea una expulsión.

Las demás peticiones quedan para registros posteriores: `until` y `async` en un bloque (#46203, #28997) no tienen una reescritura exacta que conserve los intentos y la espera, y un `import_playbook` condicional (#34281), una verbosidad por tarea (#24215), Y y O en las expresiones de tags (#11396) y una lista de direcciones para `ansible_host` (proposals#97) necesitan antes su propia medición.

### Las reproducciones shadow y el lock

Una [reproducción shadow](/es/docs/start/switch-without-risk/) reproduce una grabación de ansible-core, y ansible-core rechaza una extensión, así que una extensión nunca aparece en una grabación. Un proyecto que quiere ambas cosas mantiene sus playbooks portables mientras graba y reproduce, y empieza a usar extensiones una vez hecho el cambio. También puede grabar la salida de `volant eject` y reproducir esa. El lock sigue la misma regla: fija el motor que ejecuta un proyecto y no dice nada de las extensiones que usa.

## Consecuencias

- Un proyecto puede dejar Volant en cualquier momento: `volant eject` le devuelve un árbol que ansible-core ejecuta, y `check --portable` lo demuestra.
- Cada extensión cuesta una regla de reescritura y una prueba de ida y vuelta antes de fusionarse, y un contexto donde la reescritura no es exacta se rechaza, no se aproxima.
- La implementación sigue en estas issues, por tamaño:
  - leer las claves `volant.` y rechazar una desconocida (pequeña);
  - la pasada de reescritura, compartida por `eject` y por las pruebas de ida y vuelta (mediana);
  - `volant eject` (mediana);
  - `volant check --portable` (pequeña);
  - `volant.loop` y `volant.loop_control` en un bloque (mediana);
  - un bloque como handler (pequeña);
  - `volant galaxy collection uninstall` (pequeña).

## Mediciones, 2026-10-25

ansible-core 2.19.12, dos hosts con conexión local, `gather_facts: false`. El bucle reescrito incluye tres tareas, `create`, `once` (con `run_once: true`) y `key` (con `when: item != 'bob' or inventory_hostname == 'h1'`), sobre `[alice, bob]`.

Bajo `linear`, primero vienen las inclusiones y luego las tareas de cada elemento por turno:

```text
TASK [per user] ****************************************************************
included: _volant/per-user.yml for h1, h2 => (item=alice)
included: _volant/per-user.yml for h1, h2 => (item=bob)
TASK [create] ******************************************************************
ok: [h1] => {"msg": "create alice on h1"}
ok: [h2] => {"msg": "create alice on h2"}
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key alice"}
ok: [h2] => {"msg": "key alice"}
TASK [create] ******************************************************************
...
TASK [key] *********************************************************************
ok: [h1] => {"msg": "key bob"}
skipping: [h2]
```

Bajo `free`, `run_once` no se respeta:

```text
[WARNING]: Using run_once with the free strategy is not currently supported. This task will still be executed for every host in the inventory list.
TASK [once] ********************************************************************
ok: [h1] => {"msg": "once alice"}
TASK [once] ********************************************************************
ok: [h2] => {"msg": "once alice"}
```

Bajo `serial: 1`, cada lote ejecuta todo el bucle, host tras host. Un bloque en `handlers:` no se puede notificar por su nombre, y su reescritura se ejecuta:

```text
TASK [change] ******************************************************************
[ERROR]: The requested handler 'restart app' was not found in either the main handlers list nor in the listening handlers list

RUNNING HANDLER [restart app] **************************************************
included: _volant/restart-app.yml for h1, h2
RUNNING HANDLER [stop] *********************************************************
ok: [h1] => {"msg": "stop"}
ok: [h2] => {"msg": "stop"}
RUNNING HANDLER [start] ********************************************************
ok: [h1] => {"msg": "start"}
ok: [h2] => {"msg": "start"}
```
