---
sourceHash: 89ab68cf559b
title: Precedencia y facts
description: Qué fuente gana cuando dos definen la misma variable, las variables mágicas, y cómo se recopilan, nombran y guardan en caché los facts.
---

Esta página responde a dos preguntas: qué valor tiene una variable cuando varias fuentes la definen, y de dónde salen los facts sobre un host.

Cuando dos fuentes definen la misma variable, gana la que está más abajo en esta lista. El orden es el de ansible-core 2.19, con cada capa de rol medida contra sus vecinas.

1. El `defaults/main.yml` de un rol
2. `group_vars/all`, del directorio del inventario y después del directorio del playbook
3. Variables de grupo del inventario, aplicadas por profundidad de grupo y después por nombre
4. `group_vars/<group>`, del directorio del inventario y después del directorio del playbook
5. Variables de host del inventario
6. `host_vars/<host>`, del directorio del inventario y después del directorio del playbook
7. Facts recopilados del host
8. `vars` del play
9. `vars_files` del play
10. El `vars/main.yml` de un rol
11. `vars` de la tarea
12. `set_fact`, resultados registrados e `include_vars`
13. Parámetros de rol, y las `vars:` de una sentencia de include dinámico
14. `--extra-vars`

[Roles](/es/docs/playbooks/roles/#variables-de-rol) explica de dónde vienen las tres capas de rol y en qué se diferencian.

Un archivo de variables que solo contiene `---` y comentarios se carga como un mapa vacío, como en ansible-core. Un archivo que contiene un escalar como `42` se sigue rechazando.

Un mapa puede tomar las claves de otro con una clave de fusión, `<<: *base`, en todos los archivos YAML que lee Volant: archivos de variables, inventarios, playbooks y argumentos de tareas. Las claves fusionadas van primero, y una clave escrita junto a `<<` reemplaza a la fusionada. En `<<: [*a, *b]` gana el primer mapa. Un valor que no es un mapa ni una lista de mapas detiene la carga, con el mensaje de PyYAML.

:::note
Un fact recopilado pierde frente a las variables propias del play. Un playbook que define una variable con el mismo nombre que un fact lee su propio valor, como en Ansible.
:::

## Variables mágicas

Un conjunto fijo de variables mágicas se sitúa encima de la vista de cada host y siempre gana:

| Variable | Contiene |
|---|---|
| `inventory_hostname`, `inventory_hostname_short` | El nombre de este host en el inventario. |
| `group_names`, `groups` | Los grupos de este host, y todos los grupos con sus hosts. |
| `hostvars` | Las variables de todos los hosts, consulta más abajo. |
| `ansible_play_hosts`, `ansible_play_hosts_all` | Los hosts vivos del play, y todos sus hosts. |
| `ansible_play_batch`, `play_hosts` | Los hosts del [lote de `serial`](/es/docs/playbooks/how-a-play-runs/#lotes-con-serial) actual. `play_hosts` está obsoleta. |
| `playbook_dir`, `inventory_dir`, `inventory_file` | Rutas de la ejecución actual. |
| `omit` | El marcador que descarta un argumento de módulo. |
| `ansible_playbook_python` | El intérprete de Python que ejecuta ansible-core para Volant. Se busca cuando una plantilla lo lee, y queda sin definir cuando no se encuentra ninguno. |
| `ansible_check_mode`, `ansible_diff_mode` | Si la ejecución lleva `--check` y `--diff`. La palabra clave `check_mode` o `diff` de un play o de una tarea no los cambia. |
| `ansible_run_tags`, `ansible_skip_tags` | Las etiquetas que la ejecución selecciona y omite, consulta [Modo check, diff y listados](/es/docs/playbooks/check-and-diff/#ansible_run_tags-y-ansible_skip_tags). |
| `ansible_forks` | El valor actual de `forks`. |
| `ansible_version` | La versión de ansible-core que Volant reproduce, consulta el [registro de decisión 0003](/es/docs/decisions/0003-ansible-version-reports-the-reference-release/). |
| `volant_version` | La versión del propio Volant. Comprueba `volant_version is defined` para distinguir los dos motores. |

Dentro de un `rescue`, `ansible_failed_task` y `ansible_failed_result` se definen como facts en el host que falló.

Cada tarea recibe también `ansible_search_path`, y la tarea de un rol recibe `role_path`. Ambas están en su sitio antes de que se resuelvan las `vars:` de la propia tarea, así que una variable de tarea construida con `lookup('template')` encuentra el `templates/` del rol. [Plantillas](/es/docs/variables/templating/#qué-hay-disponible) enumera lo que contiene la ruta de búsqueda.

## hostvars

`hostvars` contiene lo que produjeron el inventario, `--extra-vars`, `set_fact` y los facts recopilados para cada host. No incluye las variables de play ni de tarea de otro host. Se reconstruye cada vez que cambia un fact.

Todos los hosts leen un único mapa compartido en lugar de su propia copia, así que una tarea que nombra `hostvars` cuesta una consulta, no una copia del inventario.

Una tarea que lee `hostvars`, `ansible_play_hosts` o `ansible_play_batch` espera a que todos los hosts lleguen primero a ella, así que ve lo que Ansible mostraría en ese punto.

## Facts

Los facts son lo que un host informa sobre sí mismo, como su nombre, su distribución y sus direcciones.

```yaml
- hosts: web
  tasks:
    - debug:
        msg: "{{ ansible_hostname }} runs {{ ansible_facts.distribution }}"
```

Los facts recopilados tienen más rango que las variables del inventario y menos que las variables propias del play, como muestra la lista del principio de esta página.

### Cuándo se recopilan los facts

Un play recopila facts salvo que defina `gather_facts: false`. Con `gathering = explicit` en `ansible.cfg`, solo recopila un play que escribe `gather_facts`. Con `smart`, se omite un host que tiene facts recientes en la [caché de facts](/es/docs/hosts/inventories/#la-caché-de-facts). Una plantilla de `gather_facts` se renderiza una vez, cuando empieza el play.

Volant ejecuta el propio módulo `setup` de ansible-core mediante la [ruta de Python en caliente](/es/docs/internals/python/), o deja que el agente (el programa pequeño que Volant pone en cada host) recopile los facts por sí mismo, cuando el playbook demuestra que no lee nada más. Consulta [Facts nativos](#facts-nativos).

Como `setup` siempre puede recurrir a su módulo de Python, el controlador necesita ansible-core y el host necesita un intérprete de Python 3. Una ejecución que necesita Python solo para recopilar facts no instala el [runtime administrado](/es/docs/reference/runtime/), así que un controlador sin ningún ansible-core utilizable detiene antes de la primera conexión un play que recopila facts, en lugar de continuar sin ellos. Define `gather_facts: false` en los plays que no necesiten facts.

`gather_subset` y `gather_timeout` funcionan en un play, y en el módulo `setup` con `filter`, como en ansible-core.

### Dos nombres para cada fact

Cada fact se puede leer en dos sitios:

- plano, con el prefijo `ansible_`: `ansible_hostname`
- dentro de `ansible_facts`, con el nombre que devolvió el módulo: `ansible_facts.hostname`

Son variables distintas. Un `set_fact` posterior de `ansible_hostname` oculta la plana y deja intacta `ansible_facts.hostname`. Ansible se comporta igual.

### Facts nativos

Un recopilador de facts nativo es una versión escrita en Rust que se ejecuta dentro del agente, así que el host no necesita Python para él. Existe para Debian y Ubuntu. Recopila los 17 recopiladores que ansible-core ejecuta con `gather_subset: min`, los facts de procesador y memoria del recopilador `hardware` de ansible-core, y las rutas por defecto y las listas de direcciones de su recopilador `network`. Un fact que el recopilador nativo no produce, como los montajes, los dispositivos o los detalles por interfaz, se omite.

Un fact ausente solo importa si algo lo lee. Por eso, antes de la primera conexión, Volant lee cada play, rol, plantilla y definición de variable de la ejecución y enumera los facts que pueden leer. Usa el recopilador nativo solo si todos son facts que él produce. Una lectura por un nombre calculado, como `ansible_facts[item]`, `vars['ansible_' ~ name]`, `lookup('vars', ...)` o un `hostvars[host]` completo pasado a un filtro, podría llegar a cualquier fact, así que mantiene el `setup` de ansible-core.

El recopilador nativo también devuelve la tarea en un host que no puede reproducir: se rinde y se ejecuta el módulo `setup` de Python. Eso ocurre en otra distribución, con SELinux activado, con facts locales en `/etc/ansible/facts.d`, o con un nombre de host que solo conoce el DNS.

`--facts python` ejecuta siempre el módulo de ansible-core, y `--facts native` pregunta siempre al agente. `--profile` imprime cuál eligió la ejecución y por qué. Consulta [Línea de comandos](/es/docs/reference/cli/#ejecución-y-salida).

### Los facts son datos

Los facts son las propias palabras del host, así que llegan sin ninguna confianza: una plantilla dentro de un fact sigue siendo texto y nunca se renderiza. Consulta [Los valores de los hosts nunca se renderizan](/es/docs/variables/templating/#los-valores-de-los-hosts-nunca-se-renderizan).

### Conservar los facts entre ejecuciones

Con `fact_caching = jsonfile` los facts persisten de una ejecución a la siguiente, consulta [La caché de facts](/es/docs/hosts/inventories/#la-caché-de-facts).
