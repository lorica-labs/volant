---
sourceHash: 93a0f9272733
title: Plugins de acción
description: Cómo Volant ejecuta los módulos que ansible-core ejecuta mediante un plugin de acción en el controlador, en Rust o en el bridge de Python.
---

La mayoría de los módulos que trae ansible-core se ejecutan tal cual: el controlador envía un payload, el host lo ejecuta y el resultado vuelve. Unos pocos funcionan de otra manera: es el propio ansible-core quien decide qué enviar, a veces después de preguntarle algo al host, y el nombre del módulo en la tarea es solo el punto de entrada. `package` es uno: lo que se ejecuta es `apt` o `dnf`, elegido después de que el host diga cuál tiene. `copy` es otro: que algo viaje o no al host depende de un checksum que el host informa antes.

Volant ejecuta ocho de ellos por sí mismo: `copy`, `dnf`, `fetch`, `package`, `reboot`, `service`, `template` y `unarchive`. Cada uno es una pequeña máquina de estados en el controlador: se le pide el siguiente paso y se le entrega el resultado del anterior, hasta que tiene el resultado de la propia tarea. Cada paso que da es un módulo ordinario de la union de la ejecución, enviado por el enlace que la tarea ya tiene, un paso por viaje de ida y vuelta, salvo el `stat` y el `file` de `copy` descritos más abajo. El resultado con el que termina un plugin sigue el mismo camino que el de cualquier otro módulo, de modo que nada de lo que respondió un host llega a las variables de un playbook por una vía propia.

La palabra clave `timeout:` de una tarea acota así el elemento entero, en cualquiera de los ocho: cada subtarea y cada espera a que el host vuelva, incluida la reconexión de `reboot`. Si se agota, la tarea termina ahí con `Task failed: Timed out after <seconds> second(s).`, y nunca con lo que el plugin habría hecho con el tiempo que le quedaba.

Los nombres que ansible-core también ejecuta mediante un plugin de acción, y todos los plugins de acción de las colecciones del corpus, siguen un segundo camino: el Python del propio plugin, de ansible-core, en el helper del controlador. Consulta [En el bridge](#en-el-bridge) más abajo. Consulta [Módulos](/es/docs/reference/modules/) para saber qué ejecuta esta versión de forma nativa, en el controlador y en la ruta de Python en caliente.

## `copy`

Un archivo del controlador, o un `content:` escrito directamente en el destino, que se envía al host solo cuando difiere.

El plugin ejecuta primero `stat` en el destino, pidiendo un SHA-1 salvo con `force: false`, caso en el que no pide ninguno y deja intacto un destino existente, sea cual sea su contenido. Compara esa suma con la de los bytes locales. Un destino que ya tiene los mismos bytes recibe `file`, para fijar los atributos que pidió la tarea; todo lo demás recibe los bytes preparados en el host y una subtarea `copy`. Solo este último caso envía algo. El `file` va en el mismo viaje que el `stat`: el agente solo lo ejecuta si el `stat` encontró un archivo regular con esa suma, así que un destino que ya tiene los bytes cuesta un solo viaje. En un bucle de `copy` o `template` en el que cada elemento nombra su propio `dest`, sin `until`, `break_when`, `pause`, `timeout`, `async` ni delegación, los elementos dan cada paso juntos, en el mismo viaje: un bucle sobre archivos que el host ya tiene cuesta un solo viaje.

`remote_src: true` se salta todo esto: `copy` se ejecuta solo, con los argumentos de la propia tarea, y `src` nombra un archivo que ya está en el host.

Un archivo de más de 16 MiB nunca se lee entero: el plugin calcula su hash en el controlador y lo envía al host en trozos de 16 MiB, que el agente escribe en disco a medida que llegan. `unarchive` envía su archivo comprimido de la misma manera.

Se rechaza antes de preguntarle nada al host, con las palabras de ansible-core:

- `src (or content) is required`
- `dest is required`
- `src and content are mutually exclusive`
- `can not use content with a dir as dest`
- `copying a directory is not supported yet: <src>`. El plugin de Rust no copia un directorio, así que la tarea se ejecuta en el bridge, que sí lo hace.
- Un `src` que no nombra nada en el controlador, envuelto como `Unexpected AnsibleActionFail error: Could not find or access '<src>' ...`.

## `template`

Una plantilla renderizada en el controlador y entregada después al mismo camino que usa `copy`.

El plugin encuentra la plantilla bajo `templates/`, la renderiza una sola vez con las variables de la tarea y sus propios delimitadores, y copia el resultado como `copy` copia un `content:`: `stat`, y después `file` o un `copy` preparado. El renderizado ocurre exactamente una vez; nada vuelve a renderizar el resultado, así que un valor aportado por un host administrado llega al archivo como texto y no como una plantilla propia.

Se fijan cuatro variables para el renderizado, además de las de la propia tarea: `ansible_managed` (salvo que la tarea ya la haya definido), `template_path` (`src` tal como lo escribió la tarea), `template_fullpath` (la ruta encontrada en el controlador) y `template_destpath` (`dest` ya renderizado, consulta más abajo). `template_host`, `template_uid`, `template_run_date` y `template_mtime`, que ansible-core también define, no se fijan.

`trim_blocks` está activado por defecto y `lstrip_blocks` desactivado, como en ansible-core, y se conserva el salto de línea final del archivo. `newline_sequence` acepta `\n`, `\r` o `\r\n`, y la grafía escapada de cuatro caracteres se lee igual que la literal. Las seis opciones de delimitadores (`variable_start_string` y sus cinco parientes) y una primera línea `#jinja2:` en el archivo de plantilla se leen como las lee ansible-core, consulta [Renderizar un archivo de plantilla](/es/docs/variables/templating/#renderizar-un-archivo-de-plantilla). `output_encoding` escribe UTF-8, UTF-8 con firma, Latin-1, ASCII, cp1252, ISO 8859-15, UTF-16 y UTF-32, y indica que cualquier otra codificación todavía no se admite. `{% include %}` e `{% import %}` buscan en `templates/` y luego en el propio directorio, para cada entrada de `ansible_search_path`, el directorio del playbook y el de la propia plantilla, como ansible-core.

Se rechaza antes de preguntarle nada al host:

- `'state' cannot be specified on a template`
- `src and dest are required`
- `newline_sequence needs to be one of: \n, \r or \r\n`
- `Block, variable and comment start strings must be different.`
- `TemplateOverrides.<name> must be <class 'str'> instead of <class 'NoneType'>`, para una opción de delimitador vacía o nula
- `output_encoding '<encoding>' is not supported yet on 'template': only utf-8, ... are`
- Un `src` que no nombra nada en el controlador, con las palabras de ansible-core, esta vez sin la envoltura `AnsibleActionFail`.

## `package`

El gestor de paquetes del host determina el módulo que se ejecuta de verdad.

El nombre sale de `use:`, salvo que sea `auto`; después de la variable `ansible_package_use` del host; después de `ansible_facts.pkg_mgr` del host en el que se ejecuta el módulo; después de un `setup` filtrado a ese único fact, que se ejecuta de nuevo en cada tarea y nunca se conserva. El módulo que elija el nombre es el que se ejecuta, con `use` retirado de sus argumentos; un `setup` que falla termina la tarea con `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`, y un nombre que no se resuelve a nada que esta versión pueda despachar falla con `Could not find a matching action for the "<name>" package manager.` Un nombre que informa un host es siempre solo una clave de búsqueda en esta lista cerrada: `setup`, `apt`, `dnf`, `dnf5`. La union lleva los cuatro backends y no solo el que la ejecución acabe necesitando, porque nada se construye una vez conocidos los facts: un plugin solo puede elegir entre lo que ya viajó.

## `dnf`

De nuevo el gestor de paquetes del host, esta vez eligiendo entre dos nombres de módulo en lugar de cuatro.

El nombre del backend sale de `use:`, o de `use_backend:` cuando `use` falta; los dos juntos se rechazan: `parameters are mutually exclusive: ('use', 'use_backend')`. `auto` y `yum` leen `ansible_facts.pkg_mgr` del host en el que se ejecuta el módulo, recopilado mediante un `setup` filtrado a ese único fact cuando todavía no se conoce; cualquier otro nombre se toma tal cual. `yum`, `yum4` y `dnf4` ejecutan todos el módulo `dnf`, `dnf5` ejecuta `dnf5`, y un nombre que sigue sin ser ninguno de estos falla con el mensaje de dos líneas de ansible-core, llave de cierre incluida:

```text
Could not detect which major revision of dnf is in use, which is required to determine module backend.
You should manually specify use_backend to tell the module whether to use the dnf4 or dnf5 backend})
```

Un `setup` fallido termina la tarea con su propia causa, `Failed to fetch ansible_pkg_mgr to determine the package action backend: <msg>`. Cuando la tarea no está delegada, su resultado lleva `ansible_facts.pkg_mgr` con lo que respondió el `setup`, como hace el resultado del propio ansible-core; el resultado de una tarea delegada no lleva ninguno, porque la respuesta de un delegado acabaría en los facts del host equivocado.

El módulo `yum` no es un módulo propio en ansible-core 2.19: `yum`, `ansible.builtin.yum` y `ansible.legacy.yum` son redirecciones a la acción `dnf`, y una tarea que nombra uno de ellos ejecuta este plugin. Una clave de `module_defaults` para `yum` se aplica a una tarea `dnf`, y al revés.

## `fetch`

Un archivo leído de un host, escrito en el controlador bajo `dest` y en ningún otro sitio.

Sin `become`, el plugin ejecuta primero `stat` en el origen y deja en paz un archivo local que ya coincide; bajo `become` va directamente a `slurp`. Los bytes vuelven mediante `slurp`, decodificados de forma estricta, nunca renderizados y nunca puestos en una variable. Un archivo de más de 2 MiB, cerca del máximo que `slurp` transporta, se vuelve a leer en su lugar en trozos de como máximo 16 MiB, directamente en un archivo junto al destino. Bajo `become` eso ocurre cuando `slurp` dice que el archivo es demasiado grande, tras un `stat` para su suma. El destino es `dest/<inventory_hostname>/<src>`, salvo que `flat: true` nombre directamente `dest` mismo o, con una barra final, `dest/<basename of src>`.

Medido en ansible-core 2.19.12, un `src` relativo que sube con `..` hace que ansible-core escriba fuera de `dest` en el controlador: un `src` de `../../../../../../../../tmp/x` sin `flat` escribe `/tmp/x`. Aquí se construye y normaliza cada ruta candidata sin tocar el sistema de archivos, y se rechaza salvo que esté bajo `dest`, antes de crear siquiera un directorio:

- `the fetched path <p> is outside '<dest>'`

Esto se comprueba dos veces: una sobre el `src` que escribió el playbook, y otra sobre la ruta que devuelve el host, que no tiene por qué coincidir. Con la protección activada, un `dest` cuyo renderizado leyó algo de un host administrado se rechaza antes de preguntar nada, salvo que lo cubra `[volant] host_named_dest_dirs` (consulta [Un dest nombrado por un host administrado](#un-dest-nombrado-por-un-host-administrado)), y lo mismo ocurre con el `inventory_hostname` de un bucle cuando lo fijó un host. Un `inventory_hostname` que no es un único componente de ruta se rechaza siempre:

- `the 'dest' of this task was named by a managed host, and a controller path a host chose is never written unless host_named_dest_dirs names a directory above it`
- `the inventory_hostname '<name>' is not one path component, and fetch files what it fetches under it`

Un enlace simbólico en cualquier punto bajo `dest` se rechaza en lugar de seguirse, tanto en la ruta construida a partir del playbook como en la que devuelve el host.

`fail_on_missing`, activado por defecto como en ansible-core, hace fallar la tarea ante un origen inexistente o un directorio; `fail_on_missing: false` convierte cualquiera de los dos en un resultado `ok` que lleva el mensaje de ansible-core. En ambos casos, cualquier otro fallo, como una lectura que el host deniega, sigue haciendo fallar la tarea. `validate_checksum` comprueba el SHA-1 del archivo escrito frente al del host antes de conservarlo; una discrepancia hace fallar la tarea y elimina lo escrito, a diferencia de ansible-core, que deja el archivo discrepante donde lo escribió. Un archivo que se sustituye conserva su modo existente, así que un archivo `0600` obtenido de nuevo sigue siendo `0600`.

### Un dest nombrado por un host administrado

Un `dest` lo nombra un host administrado cuando su renderizado leyó algo que aportó un host. Un caso real es `dest: "{{ tmp.path }}/out/"`, donde `tmp` es el resultado registrado de una tarea `tempfile`. ansible-core escribe ahí, y `fetch` también por defecto. Un host que responde a `tempfile` con `/home/<operator>/.ssh` hace que el controlador escriba como el operador bajo ese directorio. Lo mismo ocurre con una variable de bucle llamada `inventory_hostname` sobre una lista registrada, que nombra el directorio bajo `dest`.

Dos ajustes activan la protección, para una ejecución en la que un host es menos fiable que la persona que ejecuta el playbook. El primero es `[volant] host_named_dest_dirs` (entorno: `VOLANT_HOST_NAMED_DEST_DIRS`), una lista de directorios absolutos separados por dos puntos, sin definir por defecto. El segundo es `[volant] host_named_paths` con el valor `contained` o `refuse`, que actúa como una lista vacía cuando `host_named_dest_dirs` no está definido, de modo que se rechaza todo `dest` de este tipo. Una lista definida se aplica siempre, diga lo que diga `host_named_paths`, y un valor vacío rechaza todo `dest` de este tipo. Con la protección activada, un `dest` nombrado por un host solo se escribe cuando la ruta a la que se resuelve en el sistema de archivos queda estrictamente dentro de uno de los directorios:

- El `dest` debe ser absoluto y no contener `..`. Una ruta relativa y una ruta que empieza por `~` se rechazan, nunca se expanden.
- La parte más profunda de la ruta que existe se resuelve a través de todos los enlaces, y se le añade el resto. Se rechazan un enlace que sale del directorio, un enlace que no apunta a nada, el propio directorio y un hermano que comparte su nombre como prefijo (`/srv/fetch-evil` para `/srv/fetch`). La comprobación se repite justo antes de escribir.
- La escritura desciende desde el directorio de la lista y rechaza cualquier enlace por debajo, incluso uno que se quede dentro, y cualquier directorio existente por debajo que pertenezca a otro usuario o en el que todos los usuarios puedan escribir sin el sticky bit. De lo contrario, otro usuario podría cambiar cualquiera de los dos por un enlace entre la comprobación y la escritura. Los directorios que `fetch` crea por el camino son suyos.
- Un directorio de la lista en el que el grupo u otros usuarios pueden escribir no admite nada, salvo que tenga el sticky bit, como `/tmp`. Esto se comprueba cuando una tarea usa el directorio.
- Una entrada que es relativa, vacía, escrita con `~` o `..`, o `/`, detiene la ejecución al inicio con el código de salida 5.
- Un `dest` que escribió el playbook no queda sujeto a la lista, y el ajuste no cambia nada para `src`, que nombra un archivo del host.

```
the 'dest' of this task was named by a managed host, and '/home/op/.ssh/x/' is not inside any of the directories (host_named_dest_dirs = /srv/fetch)
```

Nombra un directorio en el que solo pueda escribir el operador. Con `/tmp` en la lista, un host puede elegir cualquier nombre bajo él.

Solo `fetch` queda sujeto a la lista. `copy` y `template` con `delegate_to: localhost`, y `synchronize` en modo pull, también escriben un `dest` en el controlador. No se rechazan cuando lo nombró un host, como en ansible-core y como antes.

## `service`

El sistema de init del host determina el módulo, igual que el gestor de `package`: `use:` en minúsculas salvo `auto`, después `ansible_facts.service_mgr`, después un `setup` filtrado a `ansible_service_mgr`. A diferencia de `package`, un nombre que ninguno de `systemd`, `systemd_service`, `sysvinit` lleva recurre a `service` en lugar de hacer fallar la tarea, y lo mismo ocurre con un `setup` que falló. Nombrar `systemd` retira los argumentos que ese módulo no admite: `pattern`, `runlevel`, `sleep`, `arguments`, `args`, cada uno con su propio aviso, `Ignoring "<name>" as it is not used in "systemd"`; una tarea que lo escribe como `ansible.builtin.systemd` los conserva.

Una ejecución que nombra tanto `package` como `service` lleva todos los backends de ambos en la union antes de que se conozca un solo fact. Medido con el propio constructor de payloads de ansible-core 2.19.12, eso supone unos 170 KB más enviados una vez por enlace de lo que costarían `apt` y `systemd_service` solos.

## `unarchive`

Un archivo comprimido leído en el controlador, o que ya está en el host, extraído por el módulo `unarchive`.

`copy:` se pliega en `remote_src` (la inversión del propio ansible-core), y los dos juntos se rechazan: `parameters are mutually exclusive: ('copy', 'remote_src')`. Un `creates:` que ya existe en el host omite la tarea, comprobado con un `stat` antes incluso de mirar `dest`: `{"changed": false, "msg": "skipped, since <path> exists", "skipped": true}`; un `stat` que falla se lee como "no está", igual que una prueba de existencia en el shell leería un comando que no pudo ejecutar, y no como un fallo de la tarea. El destino debe existir y ser un directorio, o la tarea falla con `dest '<dest>' must be an existing dir`. Solo entonces se busca el origen, se rechaza si lo nombró un host, se prepara y se envía, salvo que `remote_src` nombre un archivo que ya está allí.

Se rechaza antes de preguntarle nada al host:

- `src (or content) and dest are required`
- `a '~' path is not supported yet on 'unarchive'`, para `dest` o `creates`. La tarea se ejecuta entonces en el bridge.
- Un `src` que no nombra nada en el controlador: `Task failed: Could not find or access '<src>' ...\nIf you are using a module and expect the file to exist on the remote, see the remote_src option`
- `src is a directory, not an archive: <src>`, para un `src` que se encuentra como directorio en el controlador.

## `reboot`

Reinicia el host y espera a que responda con un nuevo boot id y a que su comando de prueba tenga éxito, antes de que termine la tarea.

Leído en `plugins/action/reboot.py` de ansible-core 2.19.12 y medido frente a él, bajo `become`, en un host que volvió por sí solo: un `setup` filtrado a la distribución, `cat /proc/sys/kernel/random/boot_id` ejecutado como comando simple, un `find` por cinco rutas de búsqueda para el programa de apagado, el propio comando de apagado, después el boot id leído de nuevo hasta que cambia y, por último, el comando de prueba. Todo esto se ejecuta por la conexión existente del host, mediante `raw`, igual que el comando de bajo nivel de ansible-core se ejecuta por la suya.

Ejecutado contra `local`, la conexión que usa el propio controlador, la tarea se rechaza antes que nada: `Running reboot with local connection would reboot the control node.`

Una vez enviado el comando de apagado, esta versión cierra la conexión y abre una nueva para comprobar el host, en lugar de mantener abierta la antigua durante el reinicio. `post_reboot_delay` es lo que espera antes de la primera comprobación, y `reboot_timeout` (600 segundos por defecto) acota la espera entera salvo que el `timeout:` de la propia tarea la corte antes. `connect_timeout`, cuando la tarea lo indica, solo fija el `ConnectTimeout` de SSH de cada reconexión; ya no acota el intento en sí. Una respuesta vacía del boot id no se lee como un nuevo arranque: un host puede no responder nada en el momento justo antes de apagarse. Una vez que el boot id ha cambiado, `test_command` (`whoami` por defecto) aún tiene que tener éxito antes de que la tarea termine; si se agota cualquiera de las dos esperas, la tarea falla con el mensaje de ansible-core, `Timed out waiting for <what> (timeout=<reboot_timeout>)`. Un enlace que otro host de la misma ejecución mantuvo abierto con el que acaba de reiniciarse recibe una comprobación de actividad adicional antes de su próximo uso, y se sustituye si el reinicio lo mató: este driver solo cierra sus propios enlaces, no los de un delegado.

La línea de apagado sigue la tabla del propio ansible-core: `shutdown -r <minutes> "<msg>"` en la mayoría de las distribuciones, `reboot` a secas en Alpine, `shutdown -r +<minutes> "<msg>"` en Void, o `reboot_command` dividido en su primer espacio cuando la tarea lo indica. `search_paths` (cinco directorios por defecto) es donde se busca el programa de apagado cuando `reboot_command` no nombra ya una ruta absoluta; no encontrarlo allí hace fallar la tarea, nombrando todas las rutas buscadas.

Se rechaza antes de preguntarle nada al host: un argumento fuera de los nueve de ansible-core (`boot_time_command`, `connect_timeout`, `msg`, `post_reboot_delay`, `pre_reboot_delay`, `reboot_command`, `reboot_timeout`, `search_paths`, `test_command`) hace fallar la tarea, ordenado y nombrado: `Invalid options for reboot: <name>[,<name>...]`.

## En el bridge

`add_host`, `assemble`, `gather_facts`, `group_by`, `script`, `set_stats`, `uri` y `wait_for_connection` ejecutan el propio plugin de acción de ansible-core en el helper, y lo mismo ocurre con cada plugin de acción que trae una colección (salvo los plugins que necesitan un host Windows u otro tipo de conexión; la página de [paridad](/es/docs/reference/parity/) los cuenta). `copy` y `unarchive` toman el mismo camino para las dos formas que rechazan los plugins de Rust: `copy` de un directorio, y `unarchive` con un `~` en `dest` o `creates`. Con `host_named_paths = refuse`, un `src` nombrado por un host administrado nunca lo toma: el rechazo del plugin de Rust se mantiene. `async_status` no es un plugin conectado mediante el bridge: lo ejecuta el agente, y un plugin conectado mediante el bridge consulta sus trabajos a través de él.

El plugin pide lo que necesita y el controlador responde: ejecuta este módulo, ejecuta este comando, lee una variable, crea un directorio temporal, pon este archivo. Cada respuesta queda acotada por el `timeout:` de la tarea. Un plugin que nunca devuelve el control, o un helper que muere, hace fallar esa tarea y no la ejecución; un directorio temporal que el plugin dejó en el host se elimina. `add_host`, `group_by` y `set_stats` no necesitan ningún host y se ejecutan sin conexión. Lo que devuelven lo aplica el ejecutor: hosts y grupos nuevos para las tareas siguientes, y las estadísticas personalizadas que se imprimen tras el recap. Un valor de `add_host` renderizado a partir de un fact sigue siendo un dato: se conserva como fact del nuevo host, nunca como variable de inventario. Lo mismo ocurre con un nombre de host o de grupo de `add_host` o `group_by` construido a partir de texto que escribió un host administrado. Y con cualquier nombre de host o de grupo que contenga `{`, lo hayan dado los archivos de inventario, una fuente de inventario dinámica, `meta: refresh_inventory`, `add_host` o `group_by`: `groups`, `hostvars`, `group_names`, las listas de hosts de un play e `inventory_hostname` lo muestran como texto, igual que ansible-core. Una vez que existe un nombre así, toda lectura de las listas que lo llevan es un dato. Leer un host mediante `hostvars[name]` es un dato solo para ese host, así que una plantilla anidada de otro host sí se renderiza.

El helper hace las veces de la conexión del controlador, así que algunas cosas que ansible-core deja hacer a un plugin se rechazan, por su nombre, antes de que hagan nada:

- Un miembro de la conexión o del contexto del play que no es uno de los pocos que lleva el shim: `Connection.<name> is not supported yet`, `the connection option <name> is not supported yet`, `the play context field <name> is not supported yet`, `the shell plugin <name> is not supported yet`.
- Un modo de `async_status` distinto de `status` y `cleanup`.
- Un módulo que no es texto UTF-8 donde el shim lo lee como texto.
- Las palabras clave de tarea `action`, `local_action` y `module_defaults` dentro del helper: el controlador ya fusionó los valores por defecto.

Dos partes de la conexión del helper siguen a ansible-core:

- `wrap_async` inicia un trabajo real en el agente. Los trabajos se ejecutan en paralelo, y un trabajo que supera su límite de `async:` se mata y termina con `Timeout exceeded`.
- Un comando que un plugin envía con `sudoable=False`, el directorio temporal del plugin y los archivos que coloca se ejecutan como el usuario de la conexión, por un segundo enlace que no escala privilegios. Bajo `become`, solo los comandos `sudoable` y los módulos se ejecutan con escalado de privilegios.

### Un plugin que crea su propia conexión

`ansible.posix.synchronize` sobre ssh no ejecuta su módulo en el host. Crea una conexión `local` propia y ejecuta el módulo en el controlador, donde `rsync -e ssh` copia los archivos hacia el host o desde él. El helper da a un plugin así los métodos propios de ansible-core para ejecutar un módulo, y reserva su conexión sustituta para la conexión con la que llegó la tarea.

El plugin lee la clave y el usuario del enlace (la variable de host, `--private-key`, `-u` o `ansible.cfg`), si la tarea escala privilegios y como quién (`sudo` pasa a ser `--rsync-path='sudo -u <user> rsync'`), `ansible_port`, `delegate_to` y `ansible_delegated_vars` y, con `use_ssh_args`, los `ansible_ssh_common_args` y `ansible_ssh_extra_args` del host. `ssh_args` es el valor por defecto de ansible-core, `-C -o ControlMaster=auto -o ControlPersist=60s`: un `ssh_args` definido en `ansible.cfg` no se lee. `rsync` tiene que estar en el controlador y en el host. Push, pull, `delete`, `rsync_opts`, `delegate_to`, `become` con `sudo`, `--check` y `--diff` dan la salida que da `ansible-playbook`.

### Carencias conocidas

- `wait_for_connection` en un host que está caído desde la primera conexión termina con `UNREACHABLE` para un elemento del plugin que nunca obtuvo un enlace; ansible-core informa `failed` con `timed out waiting for ping module test`.
- Un resultado conectado mediante el bridge que es `unreachable` a nivel de resultado se imprime como `ok`.
- Un resultado omitido conectado mediante el bridge lleva `"failed": false`.
- Los avisos que un plugin escribe con `display` van al error estándar del helper, no a la salida de la tarea; lo mismo ocurre con el aviso `Error loading plugin` de ansible-core para un nombre de colección desconocido.
- Un módulo que `runtime.yml` redirige a un plugin de acción con otro nombre se ejecuta con el nombre de la propia tarea.
- `ansible_play_hosts_all` no crece para un host añadido dentro de un play con `serial`, y un `add_host` o `group_by` delegado se aplica al host de la propia tarea.

## Un archivo src nombrado por un host administrado

Un `src` lo nombra un host administrado cuando su renderizado leyó algo que aportó un host: un fact recopilado, como en `src: "{{ ansible_os_family }}.j2"`, un resultado registrado, o un elemento de `with_first_found` construido a partir de uno. `[volant] host_named_paths` dice qué hacen `copy`, `template`, `script`, `unarchive` y `assemble` con él. Se aplica al archivo leído en el controlador, y también bajo `--check` y `--diff`. Para `template`, una opción de delimitador (`variable_start_string` y las otras cinco) cuyo valor aportó un host se usa con `allow`, como la usa ansible-core, y se rechaza con `contained` y `refuse`. Una primera línea `#jinja2:` en la plantilla de un autor se lee en todos los modos.

| Valor | Qué hace la tarea |
|---|---|
| `allow` (el valor por defecto) | Busca el archivo y lo envía, como hace ansible-core. |
| `contained` | Lee el archivo solo cuando está en un directorio `files` o `templates` que la tarea recorre. |
| `refuse` | Hace fallar la tarea antes de buscar ninguna ruta. |

`allow` significa que un host administrado puede elegir qué archivo legible del controlador le envía una tarea `copy`, `template` o `script`, exactamente como en ansible-core 2.19.12. Medido allí, un `src` que es un valor registrado, el `stdout` de un comando o un fact envía el archivo que nombre, `~/.ssh` incluido. Los roles que eligen una plantilla por fact dependen de esto. `contained` y `refuse` existen para endurecerlo, en una ejecución en la que un host es menos fiable que la persona que ejecuta el playbook.

Con `contained`, los directorios que cuentan son los directorios `files` y `templates` del rol, de los roles de los que es dependencia, del directorio del archivo de la tarea (`tasks/files` en un rol) y del directorio del playbook. El resto de esos árboles no cuenta: `group_vars`, `vars`, `defaults`, un inventario, `ansible.cfg` y un archivo de contraseña de vault nunca son alcanzables con un nombre que eligió un host, y tampoco lo es un archivo guardado junto al playbook. Las reglas, por orden:

- Un nombre que empieza por `~`, o `~user`, se rechaza. Los propios plugins de ansible-core lo expanden a un directorio personal, cosa que el texto del nombre no muestra.
- Un nombre con un componente `..` se rechaza, incluso uno que vuelve a caer dentro. El sistema de archivos sigue un enlace antes de plegar `..`, así que la ruta plegada no es la que se abriría.
- Un nombre absoluto debe estar bajo uno de esos directorios y no debe contener un `$`, que ansible-core expande en un nombre absoluto. `with_first_found` entrega un nombre absoluto de este tipo.
- Un nombre relativo se busca como lo busca el plugin. El primer archivo que existe es el que se lee, y debe estar bajo uno de esos directorios. Un archivo con ese nombre junto al playbook, fuera de `files`, se rechaza cuando existe. Como consecuencia, el rechazo le dice a un host que ese archivo existe. Esto se limita a un archivo en `<playbook>/<name>`, `<role>/<name>`, `<role>/tasks/<name>` o `<parent role>/<name>`; la respuesta para cualquier otro nombre es el habitual "could not find".

La comprobación lee el texto de la ruta. Un enlace que pusiste dentro de esos directorios es tu propio archivo y se sigue. Un origen en el host, como `remote_src: true` para `copy` y `unarchive`, o `assemble` cuando `remote_src` no está en false, no es un archivo del controlador y no se comprueba. `template`, `script` y los plugins de una colección no leen `remote_src`, así que nunca los exime. Para `script`, el archivo es la primera palabra de la línea, o de `cmd`. Para `uri`, cuentan `src` y el `filename` de cada campo de un `body` multipart.

Una tarea se rechaza antes de buscar la ruta cuando el modo o el texto del nombre lo deciden, porque una respuesta de "no encontrado" ya le diría a un host si una ruta existe en el controlador. Con `refuse`, el mensaje es:

```text
the 'src' of this task was named by a managed host, and a controller file a host chose is
never sent
```

Con `contained`, nombra el valor y el ajuste:

```text
the 'src' of this task was named by a managed host, and '../up.j2' is not inside the
files or templates directories of the playbook or the role (host_named_paths = contained)
```

El argumento es `_raw_params` o `cmd` para un `script`. Cualquier otra palabra para el ajuste detiene la ejecución al inicio con el código de salida 5 y nombra la clave, las tres palabras y el valor encontrado.

El ajuste cubre las tareas enumeradas arriba. No cubre `include_vars`, `vars_files` ni un lookup como `file` cuya ruta nombró un host. Tampoco cubre un `copy` con `remote_src` ni un `slurp` delegado a `localhost` que lee una ruta del controlador nombrada por un host. Estos se comportan como en ansible-core, y igual en todas las versiones anteriores de Volant.

El modo cambia qué archivo se lee, nunca cómo se confía en el texto de una plantilla. Cuando el `src` de un `template` lo nombró un host, `template_path` y `template_fullpath` son valores de ese host en el renderizado, igual que `template_destpath` lo es para un `dest`.

Un `fetch` cuyo `dest` nombró un host también sigue este ajuste, aunque su `dest` es un lugar donde escribe el controlador y no un archivo que lee: `allow` lo escribe, y `contained` y `refuse` lo escriben solo dentro de `host_named_dest_dirs` (consulta [Un dest nombrado por un host administrado](#un-dest-nombrado-por-un-host-administrado)). El `dest` de los demás plugins es siempre un lugar donde escribir en el host, así que no se rechaza; `template_destpath` lo lleva al renderizado como valor del propio host.

## Lo que se queda en el host

Un archivo viaja a un host solo cuando una subtarea realmente lo necesita, y nunca más allá de esa tarea.

El controlador lo prepara como un blob direccionado por contenido, nombrado por el hash BLAKE3 de sus bytes, en un directorio privado de la conexión que lo envió: `stage-<host>-<pid>/`, dentro de la propia caché del agente, con el modo 0700 y rechazado salvo que el agente sea su único dueño. El módulo lee el archivo desde ahí, con un nombre que elige el propio agente, nunca la ruta del controlador. Una vez que se ejecutó la subtarea que lo necesitaba, haya tenido éxito, fallado o consumido el propio módulo el archivo (como hace `copy` cuando mueve su origen preparado a su sitio), el agente elimina lo que queda de él. Nada espera a que termine la conexión: una plantilla renderizada suele llevar un secreto, y un archivo que nadie ha pedido por segunda vez no se deja para que alguien lo encuentre más tarde.

También se elimina el directorio de preparación entero de la conexión cuando esta termina, sea lo que sea lo que quedara en él. Un agente nuevo que arranca en un host elimina además los directorios de preparación de agentes muertos del mismo host que encuentra al iniciarse, de modo que una ejecución interrumpida no los deja para siempre. Dos enlaces a un mismo host, como dos nombres de inventario para una misma máquina o dos hosts que delegan en el mismo, nunca comparten un directorio de preparación, ya que cada uno lleva el nombre de su propio id de proceso: el archivo preparado por una conexión nunca es uno que otra conexión pueda ver.
