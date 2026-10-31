---
sourceHash: 630bc063e9af
title: Delegación, tareas meta y environment
description: Ejecutar una tarea una sola vez para un lote o en otro host, qué hace cada acción meta, y cómo dan forma a una tarea environment y no_log.
---

Esta página responde a cómo ejecutar una tarea en un lugar distinto de donde trabaja el play, cómo dirigir la propia ejecución con `meta`, y cómo definir variables de entorno u ocultar la salida de una tarea.

## run_once

`run_once` ejecuta una tarea en un host del lote y da a todos los demás hosts lo que produjo, incluido todo lo que registró.

```yaml
- name: Run database migrations once
  command: /opt/app/migrate.sh
  run_once: true
```

Los hosts del lote se encuentran delante de una tarea así antes de que ninguno la ejecute. La elección tiene que ser la misma para todos, y los demás hosts tienen que esperar al que la ejecuta.

`run_once` puede ser una plantilla. Cada host del lote la renderiza por turnos, y el primer host para el que resulta verdadera ejecuta la tarea. Una plantilla que responde de forma distinta dentro del lote termina la ejecución con el código 4.

Con `strategy: free` y `host_pinned`, `run_once` se ignora con un aviso y la tarea se ejecuta en todos los hosts. Una tarea `run_once` que falla allí solo hace fallar a su propio host. Consulta [Otras estrategias](/es/docs/playbooks/how-a-play-runs/#otras-estrategias).

## delegate_to

`delegate_to` ejecuta una tarea en un host distinto de aquel para el que se está ejecutando.

```yaml
- name: Take the host out of the load balancer
  command: /usr/local/bin/lb-drain {{ inventory_hostname }}
  delegate_to: lb1
```

La tarea viaja por la conexión propia del delegado, con los ajustes de conexión del delegado. El resultado sigue perteneciendo al host con el que trabaja el play: el recap cuenta ese host, y la línea de resultado dice `[web1 -> lb1]`.

- Una tarea omitida por `when` nunca llegó al delegado, así que su línea `skipping:` no lleva flecha.
- La conexión propia del host que delega no se abre para una tarea delegada, así que una tarea delegada desde un host inalcanzable se ejecuta igualmente.
- El delegado se busca en todo el inventario, no solo en los hosts del play. Un play sobre dos hosts puede delegar en un tercero que queda fuera del recap.
- Un nombre que el inventario no tiene se trata como un destino SSH con ese nombre, y se informa como inalcanzable si eso falla. Un delegado inalcanzable saca de la ejecución al host que delega.

`delegate_facts: true` guarda lo que produjo la tarea en el delegado en lugar de en los hosts que la ejecutaron.

Un módulo que se ejecuta en el controlador, como `set_fact` o `debug`, se ejecuta siempre en el controlador, diga lo que diga `delegate_to`. El delegado solo decide entonces el nombre que se muestra en la línea de resultado y, con `delegate_facts`, de quién se escriben los facts.

`action:` y `local_action:` aceptan una cadena (`command echo hi`), un mapa con `module:` que Ansible considera obsoleto, o un nombre de módulo solo con los argumentos bajo `args:`. Una palabra como `module=command` tras `action:` no nombra ningún módulo. La tarea falla cuando un host llega a ella, con el código 2, y un `rescue` o `ignore_errors` absorbe el fallo, como en Ansible.

`delegate_to` y `delegate_facts` pueden escribirse en un bloque y en una tarea, no en un play, que es donde Ansible también los acepta. Las dos palabras clave se combinan: una tarea `run_once` con un `delegate_to` se ejecuta una vez, en el delegado.

## Tareas meta

`meta` pide algo al motor en lugar de ejecutar un módulo. Se ejecutan aquí todas las acciones que tiene Ansible 2.19, y cada una imprime un banner `TASK [meta]`. Una acción que Ansible no tiene imprime el banner y después falla con `invalid meta action requested: <action>` y un bloque `Origin`, con el código 1, incluso cuando su `when` es falso. Una tarea meta no añade nada al recap: su línea `skipping:` tampoco se cuenta.

| Acción | Qué hace | De quién cuenta el `when` |
|---|---|---|
| `flush_handlers` | Ejecuta los handlers de los que se ha avisado al host, donde está la tarea. | El de cada host. |
| `noop` | Nada. | Se ignora, con un aviso. |
| `end_host` | El host abandona el play. Sin más tareas, sin secciones `always`, sin handlers. | El de cada host. |
| `end_role` | El host omite el resto del rol en el que está, incluida una sección `always` del rol. | El de cada host. |
| `end_batch` | El lote termina para todos los hosts, y empieza el siguiente lote. | El del primer host. |
| `end_play` | El play termina para todos los hosts, con los lotes que queden. | El del primer host. |
| `clear_facts` | Olvida los facts que reunieron los módulos, en todos los hosts del lote. Los facts de `set_fact` se quedan. | El del primer host. |
| `clear_host_errors` | Perdona los fallos y los hosts inalcanzables del lote. Consulta más abajo. | El del primer host. |
| `refresh_inventory` | Vuelve a leer todas las fuentes de inventario. Consulta más abajo. | Se ignora, con un aviso. |
| `reset_connection` | Cierra la conexión del host, y con ella la conexión `ssh` compartida. La siguiente tarea abre ambas de nuevo. | Se ignora, con un aviso. |

"El primer host" es el primer host del lote que sigue en el play, en el orden del inventario. Las acciones que leen su `when` se ejecutan una vez e imprimen un banner. Las demás se ejecutan para cada host e imprimen un banner por cada host que sigue en el play.

`end_role` fuera de un rol detiene la ejecución antes de que empiece, con el código 4 y el mensaje de ansible-core.

### Hosts que se van

Un host que termina con `end_host`, `end_batch` o `end_play` no ha fallado, y el recap lo muestra tal como estaba. Dentro de un `rescue`, el fallo que rescató se cuenta como rescatado y el host queda fuera del código de salida. Un host que ya había fallado conserva su fallo. Los plays posteriores se ejecutan de nuevo en el host.

### Perdonar errores

`clear_host_errors` quita los fallos del lote del código de salida. El recap conserva sus cuentas, así que un host puede mostrar `failed=1` en una ejecución que termina con 0.

- Un host que falló no vuelve al play en el que falló. Vuelve en los plays posteriores.
- Un host que pasó a ser inalcanzable vuelve. Retoma el play donde se quedó y ejecuta los pasos que se perdió cuando todos los demás hosts han terminado. Si vuelve a ser inalcanzable, se cuenta de nuevo.
- Un host que fue inalcanzable en un play anterior también vuelve, desde la primera tarea del play que lo perdonó. Vuelve a ser inalcanzable en los plays posteriores, y el código de salida dice 4 en cualquier caso.

### Actualizar el inventario

`refresh_inventory` vuelve a leer todas las fuentes de inventario, en el orden de `-i`. `groups` y `hostvars` cambian de inmediato. Un host que las fuentes ganaron se une desde el siguiente play, y un host que perdieron abandona el play donde está, sin fallar. `--limit` también se resuelve de nuevo, así que un host que nombra y que solo tienen las fuentes nuevas también se une. Los hosts y grupos creados por `add_host` y `group_by` permanecen. Si las fuentes ya no se pueden leer, el inventario queda vacío a partir de entonces, como en Ansible: todos los hosts se van y los plays siguientes no coinciden con nada.

Los facts y los valores de `set_fact` se quedan como estaban. Consulta [Inventarios](/es/docs/hosts/inventories/).

## environment

`environment` define variables para el proceso en el que se ejecuta un módulo.

```yaml
- hosts: web
  environment:
    http_proxy: http://proxy.internal:3128
  tasks:
    - name: Fetch the release
      command: curl -fsSLO https://example.com/app.tar.gz
      environment:
        CURL_CA_BUNDLE: /etc/ssl/certs/internal.pem
```

Las capas se combinan clave por clave, cada una sobre la anterior: primero el play, después cada bloque envolvente de fuera hacia dentro y por último la tarea. Cada capa se renderiza con las variables del elemento de bucle actual.

Los valores pasan a ser lo que `str()` de Python hace de ellos, así que `42` pasa a `"42"`, `true` a `"True"` y `null` a `"None"`. Una variable sin definir dentro de `environment` hace fallar la tarea.

`environment` también puede ser una lista de mappings, que se combinan en orden. Una entrada o un valor que sea `omit` se omite.

## no_log

`no_log: true` sustituye la salida de la tarea por la línea censurada que imprime `ansible-playbook`, en todas las líneas que habría mostrado y en todos los niveles de verbosidad. Eso cubre las líneas `ok:` y `changed:`, `fatal:`, `UNREACHABLE!`, la línea de cada elemento de bucle y la salida de `debug`. La etiqueta de un elemento de bucle pasa a ser `(censored due to no_log)`.

El bloque `[ERROR]:` que se imprime encima de un fallo conserva su prefijo, como `Task failed: Action failed:`, y muestra `<error censored due to no log>` en lugar del mensaje. Aquí Volant se diferencia de `ansible-playbook` 2.19.12, que censura solo el motivo de un host inalcanzable e imprime completo el mensaje de una tarea fallida, de modo que un secreto en el mensaje de un `fail` llega al terminal.

`no_log` puede ser una plantilla. Cada host la renderiza cuando se ejecuta la tarea. En un bucle, un elemento que oculta oculta todos los elementos, mientras que ansible-core oculta cada elemento según su propio valor.

El banner de la tarea y su nombre no se ocultan. La variable registrada y el recap conservan el resultado real, así que una tarea posterior todavía puede leerlo.

## Diferencias con Ansible

- `delegate_to` se renderiza una vez por tarea, no una vez por elemento de bucle. Un lote es un mensaje a un agente por una conexión, y un delegado por elemento repartiría un bucle entre conexiones.
- `local_action` es `delegate_to: localhost` escrito de otra manera, y gana a un `delegate_to` de la misma tarea.
- Cuando no se puede llegar al host elegido para ejecutar una tarea `run_once`, los hosts que esperan abandonan el play y la ejecución termina allí. Ansible imprime un banner de play vacío más. Ninguno de los dos motores elige otro host, y el código de salida es el mismo 4.
- `run_once` combinado con `delegate_facts` guarda los facts en el delegado, lo que hace que `delegate_facts` signifique una sola cosa. El comportamiento de Ansible aquí no se midió.
- `become`, `become_user` y `become_method` escritos a partir del elemento del bucle se renderizan para cada elemento. Los elementos que escalan igual pasan por un solo enlace; un elemento que escala de otra forma pasa por un enlace propio, así que `become: "{{ item }}"` sobre `[true, false]` ejecuta el primer elemento como root y el segundo como el usuario de la conexión, como en Ansible. Un bucle así ejecuta sus elementos de uno en uno.
- Un valor de `environment` que no es un mapa se omite con un aviso que nombra el valor. Ansible imprime en su lugar toda la pila de capas en la que estaba.
- Ese aviso no lo censura `no_log`. Ansible tampoco censura su aviso, así que esto coincide con ansible-core, pero tenlo en cuenta antes de poner un secreto en un `environment` que podría no ser un mapa.
