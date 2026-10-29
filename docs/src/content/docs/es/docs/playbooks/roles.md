---
sourceHash: fd7538abfe30
title: Roles, includes e imports
description: Dónde busca Volant los roles, cómo funcionan las dependencias, las especificaciones de argumentos y las variables de un rol, y cómo los imports y los includes traen otros archivos a un play.
---

Esta página responde a dónde encuentra Volant un rol, cómo se comportan sus dependencias y variables, y cómo `import_*` e `include_*` traen otros archivos. Volant lee el directorio de un rol mientras se compila el play e inserta sus tareas en la lista de pasos, así que nada de un rol espera a un host.

## Encontrar un rol

Las entradas de `roles:`, las dependencias de `meta/main.yml` e `import_role` nombran un rol de la misma manera. Volant busca en este orden:

1. `<playbook_dir>/roles`
2. `roles_path` de `ansible.cfg`, o `ANSIBLE_ROLES_PATH`. Sin ninguno de los dos, los valores por defecto son `~/.ansible/roles`, `/usr/share/ansible/roles` y `/etc/ansible/roles`.
3. el propio `<playbook_dir>`

Una entrada relativa de `roles_path` se resuelve respecto al directorio de `ansible.cfg`, o respecto al directorio de trabajo cuando viene de `ANSIBLE_ROLES_PATH`. Cada entrada se convierte en absoluta, sin `.` ni `..`, antes de usarse. El `ansible_search_path` de un rol y las líneas `included:` muestran esa ruta, como hace ansible-core.

Un nombre de tres partes como `acme.demo.hello` se busca también en `ansible_collections/acme/demo/roles/hello`, primero en un directorio `collections/` junto al playbook y después en cada entrada de `collections_path`.

Un rol que nadie encuentra detiene la ejecución con el código de salida 1, y el mensaje enumera todas las rutas probadas.

## Dentro de un rol

`defaults/` y `vars/` se leen primero como archivo y después como directorio: `vars/main.yml` si existe, y si no todos los archivos de `vars/main/`, por orden de nombre. `tasks/`, `handlers/` y `meta/` aceptan un solo archivo y nunca un directorio. En la entrada de un rol, `tasks_from`, `vars_from`, `defaults_from` y `handlers_from` eligen un archivo distinto de `main`.

El nombre es una ruta dentro del directorio correspondiente, con o sin extensión (`install`, `install.yml`, `sub/install`). Un nombre que sale de ese directorio, como `../../../x` o una ruta absoluta fuera de él, detiene la ejecución con `Failed loading '...' for role (name) as it is not inside the expected role path`. Volant sigue un enlace simbólico dentro del directorio adonde apunte, como hace ansible-core.

El directorio `handlers/` de un rol se lee junto con sus tareas. En un flush, los handlers del rol se ejecutan antes que los del propio play.

## Dependencias

Las `dependencies` de `meta/main.yml` son roles por derecho propio. Se ejecutan antes del rol que las pidió, y una dependencia compartida por dos roles se ejecuta una sola vez. `allow_duplicates: true` desactiva esa deduplicación para un rol.

La deduplicación mira algo más que el nombre. Los parámetros de la entrada, sus `vars:`, su `when:` y sus selecciones `*_from` forman parte de su identidad:

```yaml
roles:
  - base                    # runs once...
  - base                    # ...because this is the same entry
  - { role: base, p: v }    # runs again: different parameters
```

## Especificaciones de argumentos

`meta/argument_specs.yml` se convierte en una tarea propia. Se ejecuta después de las dependencias del rol y antes de su primera tarea, y comprueba los argumentos del rol con la entrada de la especificación que lleva el nombre de su archivo de tareas.

La tarea lleva la `short_description` del rol tras un guion, así que `--list-tasks` muestra `Validating arguments against arg spec 'main' - The spec role`. Está etiquetada `always`, así que ninguna selección con `--tags` la descarta.

Las comprobaciones se ejecutan en el orden en que las ejecuta Ansible: un argumento obligatorio ausente, después un tipo, después un valor fuera de `choices` y después un argumento que la especificación no nombra. Las subopciones, `aliases`, `default` y `mutually_exclusive` todavía no se comprueban.

## Variables de rol

Un rol aporta tres capas de variables, y cada una está a una altura distinta del [orden de precedencia](/es/docs/variables/precedence/):

| Capa | Dónde se sitúa |
|---|---|
| `defaults/main.yml` | En lo más bajo, por debajo de las variables propias del inventario. |
| `vars/main.yml` | Por encima de las `vars:` del play, por debajo de las de una tarea. |
| Parámetros del rol, las claves libres de una entrada como `{ role: base, p: v }` | Por encima de los facts. Un parámetro de rol gana a un `set_fact` con el mismo nombre. |

:::tip
Las `vars:` escritas en la entrada de un rol no son un parámetro de rol. Viajan como una palabra clave de tarea normal. Las dos formas se parecen, así que comprueba cuál usaste cuando un rol lea un valor que no esperabas.
:::

Los `defaults` y `vars` de un rol son visibles para todo el play, así que `pre_tasks` y los roles posteriores pueden leerlos. Para los pasos del propio rol, sus valores se vuelven a poner por encima. Los parámetros del rol se quedan dentro del rol.

Un `import_role` con `public: false` los mantiene para sí, y un `include_role` hace lo mismo salvo que indique `public: true`. Consulta [Opciones de rol](#opciones-de-rol).

## Imports e includes

Cinco sentencias traen trabajo desde otro archivo. Se dividen en dos familias:

| Sentencia | Tipo | Cuándo lee Volant el archivo | Qué muestra un listado |
|---|---|---|---|
| `import_tasks` | estática | mientras se compila el play | todas las tareas de dentro |
| `import_role` | estática | mientras se compila el play | todas las tareas de dentro |
| `import_playbook` | estática | mientras se compila el play | todos los plays de dentro |
| `include_tasks` | dinámica | cuando un host llega a la sentencia | solo la sentencia |
| `include_role` | dinámica | cuando un host llega a la sentencia | solo la sentencia |

Un include dinámico se resuelve para cada host, porque el nombre de su archivo puede depender de las variables de ese host. Los pasos nuevos se insertan justo detrás de la sentencia mientras se ejecuta el play.

### Imports estáticos

La ruta de un `import_tasks` se busca primero junto al archivo que importa y después en el directorio `tasks/` del propio rol.

`import_playbook` inserta los plays del otro archivo donde está la sentencia, y cada play importado conserva el directorio del archivo del que viene. El nombre del archivo se renderiza con las variables de `-e` y mediante la [ruta de Python para filtros y lookups](/es/docs/variables/templating/#filtros-tests-y-lookups-que-necesitan-python), así que `{{ which }}.yml` funciona con `-e which=sub.yml`, y también un nombre construido con `lookup('env', ...)` o con un filtro de una colección. Todavía no hay inventario, así que un nombre no puede depender de un host. Un nombre que lee una variable sin definir detiene la carga con el código 4: `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`.

Tres palabras clave de la entrada `import_playbook` llegan a todos los plays del archivo:

- `tags` se añaden a las etiquetas de cada play importado, así que `--tags` y `--skip-tags` seleccionan sus tareas y los listados las muestran.
- `vars` se añaden a las variables de cada play importado y ganan a las del propio play.
- `when` se antepone a las condiciones de cada tarea, rol y bloque de cada play importado, y a su recopilación de facts. No se evalúa una sola vez para todo el import. Una condición falsa sigue abriendo cada play y sigue imprimiendo un skip por cada tarea y host. Los handlers no son condicionales.

Las demás palabras clave que ansible-core 2.19 acepta en la entrada, como `ignore_errors`, `become` y `environment`, no hacen nada ahí, y Volant las ignora igual. Cualquier otra clave se rechaza. También se rechaza un `tags` o un `when` que contenga una plantilla, porque Volant no tiene variables cuando carga el playbook.

### Includes dinámicos

De lo que dice la propia sentencia, solo sus `vars:` viajan al archivo incluido. Esas variables entran en el nivel de los parámetros de rol del [orden de precedencia](/es/docs/variables/precedence/), por encima de los facts y por debajo de `--extra-vars`, y se acumulan a través de includes anidados. Los `tags` de la propia sentencia se quedan en la sentencia, y su `when` se evalúa solo para ella.

Lo que dicen las capas que rodean la sentencia sí llega a las tareas incluidas: el play, la entrada del rol que trajo el archivo y los bloques que lo envuelven. Sus `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` y `vars` se aplican a todas las tareas incluidas, y el valor propio de una tarea gana al heredado. Es la regla de ansible-core 2.19, que se salta un padre cargado dinámicamente y pregunta al abuelo. Un `check_mode: true` heredado se rechaza en el archivo incluido en lugar de descartarse.

Una tarea incluida cae en el bloque y la sección donde se escribió la sentencia, así que un include que falla dentro del cuerpo de un bloque lo captura el `rescue` de ese bloque.

Volant comprueba lo que trae un include antes de ejecutarlo. Para entonces los hosts ya están conectados y las tareas anteriores se han ejecutado, así que un módulo o una palabra clave que esta versión aún no admite ya no puede detener toda la ejecución. En su lugar hace fallar la sentencia de include para el host que llegó a ella, no se ejecuta nada del archivo y un `rescue` alrededor de la sentencia puede capturarlo.

Antes de la primera conexión, Volant reúne los módulos de Python nombrados por todos los archivos de tareas y handlers de los roles del play. Un módulo nombrado solo en `setup-{{ ansible_os_family }}.yml` ya está, por tanto, en el host cuando se ejecuta el include. Un archivo de `tasks/` o `handlers/` de un rol que no se interpreta detiene la ejecución antes de que nada se conecte, para `roles:` e `import_role`. Con un `include_role`, este escaneo previo no lee el rol: los plays y tareas anteriores se ejecutan, y después la ejecución termina con el código 4 tras el banner de la sentencia, y un `when: false` en el include oculta el fallo. Consulta [La ruta de Python en caliente](/es/docs/internals/python/#cómo-funciona).

Un `notify` en un archivo incluido que no nombra ningún handler termina la ejecución con el código de salida 1 y la misma frase que da la comprobación previa, como hace ansible-core. La comprobación previa es el escaneo que Volant ejecuta antes de conectarse a ningún host. Ningún `ignore_errors` ni `rescue` puede capturarlo. Consulta [Bloques y handlers](/es/docs/playbooks/blocks/#diferencias-con-ansible).

#### Poner palabras clave en lo que trae un include

`apply:` en `include_tasks` e `include_role` pone palabras clave en cada tarea que trae la sentencia, y en los handlers de un rol incluido. Acepta las palabras clave de un bloque: `tags`, `when`, `vars`, `become`, `environment`, `ignore_errors`, `delegate_to` y el resto. Los imports rechazan `apply`, porque ya pasan las palabras clave que escribes en ellos.

```yaml
- include_tasks:
    file: install.yml
    apply:
      tags: [install]
      become: true
  tags: [always]
```

Las etiquetas de `apply` están en las tareas incluidas y no en la sentencia. Con `--tags install`, la sentencia solo se ejecuta si lleva una etiqueta que la selección conserva, aquí `always`, y después se ejecutan las tareas incluidas. Un `when` en `apply` se comprueba para cada tarea incluida, y un `when` en la sentencia se comprueba una vez. Las `vars` de `apply` son variables de bloque, así que las `vars:` de la propia sentencia ganan a las de `apply`.

Una clave de `apply` que un bloque no tiene detiene la ejecución con el código de salida 4 cuando se ejecuta la sentencia, tras su banner, como en ansible-core. Un `apply` que no es un mapa detiene la carga.

#### Opciones de rol

Con `public: true`, los `defaults` y `vars` de un `include_role` son visibles para todo lo que se ejecuta después de la sentencia, y también para los demás hosts, porque ansible-core guarda la lista de roles públicos en el play y no en el host. Una tarea que se ejecutó antes de la sentencia no lee nada de ella, y un include con `when: false` no aporta nada.

Con `allow_duplicates: false`, la sentencia muestra su línea `included:` y no ejecuta nada para un host al que ya se le ha traído un rol idéntico. Dos sentencias son idénticas cuando nombran el mismo rol, los mismos archivos y las mismas `vars:`, y, en un bucle, el mismo elemento bajo la misma variable de bucle. `rolespec_validate: false` omite la comprobación de la especificación de argumentos.

Cada opción de las cuatro sentencias, tal como las leen Volant y ansible-core 2.19.12:

| Sentencia | Opción | Valor por defecto | Volant |
|---|---|---|---|
| `include_role` | `name`, `role` | obligatorio | funciona |
| `include_role` | `tasks_from`, `vars_from`, `defaults_from`, `handlers_from` | `main` | funciona |
| `include_role` | `public` | `false` | funciona |
| `include_role` | `allow_duplicates` | `true` | funciona |
| `include_role` | `rolespec_validate` | `true` | funciona |
| `include_role` | `apply` | ninguno | funciona |
| `import_role` | `name`, `role`, los cuatro `*_from` | como arriba | funciona |
| `import_role` | `public` | `true` | funciona: `false` deja las variables dentro del rol |
| `import_role` | `allow_duplicates` | `true` | funciona, se decide mientras se compila el play |
| `import_role` | `rolespec_validate` | `true` | funciona |
| `import_role` | `apply` | ninguno | rechazado como en ansible-core, `Invalid options for import_role: apply` |
| `include_tasks` | `file` o el nombre en forma libre | obligatorio | funciona |
| `include_tasks` | `apply` | ninguno | funciona |
| `import_tasks` | `file` o el nombre en forma libre | obligatorio | funciona |
| `import_tasks` | `apply` | ninguno | rechazado como en ansible-core, `Invalid options for import_tasks: apply` |

Un valor que no es booleano en `public`, `allow_duplicates` o `rolespec_validate` hace fallar la tarea, o detiene la carga en el caso de un `import_role`, con la frase de ansible-core.

### include_vars

`include_vars` produce variables en lugar de pasos, así que no es ni estático ni dinámico. Es un módulo normal que se ejecuta en el controlador, y lo que lee queda con la precedencia de los facts.

| Pregunta | Respuesta |
|---|---|
| ¿Qué lee? | Un archivo, nombrado con `file:` o escrito como ruta en forma libre, o todos los archivos de un directorio, nombrado con `dir:`. Las dos formas aceptan opciones distintas, y dar opciones de ambas hace fallar la tarea. |
| ¿Dónde se busca un archivo? | En `vars/` y después en el propio directorio, primero para un rol y después para el playbook. Una ruta absoluta se usa tal cual. |
| ¿Dónde se busca un `dir:`? | No se busca. Una tarea de un rol lo lee bajo el `vars/` del rol, y `dir: vars/x` nombra un directorio del propio rol cuando existe. Cualquier otra tarea lo lee respecto al directorio del archivo que contiene la tarea. |
| ¿En qué orden se lee un directorio? | Los directorios ordenados por su ruta completa como texto, y después los archivos de cada directorio ordenados por nombre. Los archivos y directorios ocultos se leen, y los enlaces a directorios se siguen. |
| ¿Y si dos archivos definen la misma clave? | Un archivo posterior reemplaza por completo las claves de uno anterior. |
| ¿Qué hay en el resultado? | `ansible_facts` y `ansible_included_var_files`, los archivos en el orden en que se leyeron. Un archivo vacío no añade nada, y aun así aparece en la lista. |
| ¿Y un archivo cifrado con vault? | Una tarea que lee uno se oculta como una tarea con `no_log`: `-v` muestra la línea censurada, y un elemento de bucle que leyó uno muestra `(censored due to no_log)`. |
| ¿Modo check? | La tarea se ejecuta en modo check, como en Ansible. |
| ¿Qué falla? | Un directorio inexistente, una ruta que no es un directorio, un archivo sin una extensión válida y un archivo que no es un mapa hacen fallar la tarea con `message` definido, como en Ansible. Un fallo en un directorio conserva en el resultado las variables de los directorios anteriores al que falló, pero una tarea fallida no define variables. Un archivo que no se puede interpretar o descifrar lanza el error, y la tarea no tiene un resultado propio. Una opción del tipo equivocado falla con la frase propia de Python. |

Las opciones de la forma `dir:`:

| Opción | Significado |
|---|---|
| `dir` | El directorio que se lee. |
| `depth` | Cuántos niveles se leen, siendo el directorio el primero. `0`, el valor por defecto, los lee todos. |
| `files_matching` | Una expresión regular que se busca en cada nombre de archivo, no en su ruta. Los archivos que no coinciden se omiten. |
| `ignore_files` | Una lista de expresiones regulares, o una cadena con ellas separadas por espacios en blanco. Cada una se ancla al final del nombre. Los archivos que coinciden se omiten. |
| `extensions` | Las extensiones que puede tener un archivo, sin el punto. El valor por defecto es `yaml`, `yml` y `json`. La comparación distingue mayúsculas y minúsculas. |
| `ignore_unknown_extensions` | Omite un archivo con otra extensión. Sin ella, un archivo así hace fallar la tarea. |
| `name` | Pone todo bajo esta única variable. También funciona para un solo archivo. |
| `hash_behaviour` | `merge` superpone cada variable a la variable del mismo nombre que la tarea ya ve, con los mapas fusionados hasta el fondo. Cualquier otro valor reemplaza. |

### Límite de anidamiento

Una sentencia a 32 niveles de profundidad se rechaza. Un solo contador cubre el anidamiento de roles, `import_role` dentro de las tareas de un rol e `import_tasks` dentro de un archivo importado, porque un ciclo puede pasar por cualquier combinación de los tres.

## Diferencias con Ansible


- Un nombre de rol que contiene un `.` se prueba como ruta antes de probarlo como rol de una colección, así que un directorio de rol con un punto en el nombre sigue siendo accesible.
- `collections_path` de `ansible.cfg` y `ANSIBLE_COLLECTIONS_PATH` se leen igual que `roles_path`. Ese comportamiento no se midió contra ansible-core. El directorio `collections/` junto al playbook sí.
- Un archivo `import_tasks` inexistente se informa con las dos primeras frases de ansible-core pero no con la tercera, que nombra un número de error de Python para un módulo que nunca se llamó.
- Un `import_playbook` escrito como tarea se rechaza antes de que empiece el play en lugar de fallar bajo el banner de un play. El código de salida es el mismo 2.
- Un archivo que se importa a sí mismo se rechaza con el código de salida 1 y un mensaje propio. Ansible recurre hasta que el intérprete se rinde, con el código de salida 250 y un traceback largo.
- Una comprobación de especificación de argumentos que falla tiene `"failed": true` en su resultado, como todos los fallos que se producen en el controlador en Volant.


- Un archivo que se incluye a sí mismo llega a ese límite y se informa como un fallo de tarea con el código de salida 2. Ansible recurre hasta que se agota la pila de Python, con el código de salida 250.
- Un archivo incluido fuera de `tasks/` y `handlers/` de un rol que no se interpreta hace fallar la tarea, con el código de salida 2 y un recap. Ansible termina con el código 4 y sin recap. Un fallo que el recap contabiliza es más útil, y un `rescue` puede capturarlo.
- Un include que falla dentro de un bucle imprime la línea habitual `failed: [host] (item=...)`. Ansible imprime solo un `[ERROR]` en la salida de error estándar.
- `allow_duplicates: false` cuenta un rol como ejecutado para un host cuando se traen sus tareas. ansible-core lo cuenta cuando el host ejecutó al menos una de sus tareas, así que un rol cuyas tareas se omiten todas se ejecuta de nuevo allí.
- `allow_duplicates: false` en un `import_role` compara las sentencias mientras se compila el play, y por eso un `when` en la sentencia anterior no cambia el resultado para un host.
- Lo que produce `include_vars` queda un nivel por encima de donde lo pone Ansible. Solo lo notas con un nombre que también escribió un `set_fact`: Ansible deja que gane el `set_fact` en cualquier orden, y aquí gana la última escritura.
