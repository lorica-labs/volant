---
sourceHash: 8ebece6e812f
title: Conexiones y become
description: Cómo llega Volant a un host, las variables de host y los ajustes de ansible.cfg que lee, y cómo funciona become mediante sudo y su.
---

Esta página responde a cómo llega Volant a tus hosts, qué variables controlan la conexión y cómo se ejecutan tareas como otro usuario con `become`.

Volant llega a un host administrado, le deja un pequeño agente estático (un programa que ejecuta tus tareas y devuelve los resultados) y habla con él por la misma conexión durante el resto de la ejecución.

## Tipos de conexión

`ansible_connection` elige el transporte. Por defecto es `ssh`, como en Ansible.

| Valor | Qué hace |
|---|---|
| `ssh` | Ejecuta el cliente OpenSSH de tu `PATH`. Volant construye la línea de comandos él mismo, sin envoltorio y sin una biblioteca SSH de Rust, así que se aplica tu `~/.ssh/config`. |
| `local` | Ejecuta el agente como proceso hijo en el controlador, sin red. |
| `docker` | Ejecuta el agente en un contenedor en marcha mediante `docker exec -i`, como `community.docker.docker`. Consulta [Contenedores Docker](#contenedores-docker). |
| `podman` | Ejecuta el agente en un contenedor de Podman en marcha mediante `podman exec -i`, rootless o no, como `containers.podman.podman`. Consulta [Contenedores Podman](#contenedores-podman). |
| `chroot` | Ejecuta el agente en un árbol de directorios del controlador mediante `chroot`, como `community.general.chroot`. Consulta [Directorios chroot](#directorios-chroot). |

Cualquier otro nombre lo resuelve el ansible-core del controlador, como lo resuelve `ansible-playbook`, una vez por nombre y por ejecución. `ansible.builtin.ssh` y `ansible.builtin.local` se resuelven en los dos plugins de arriba y se ejecutan. Un nombre corto que una colección heredó de ansible-core se dirige a esa colección cuando está instalada:

| Nombre corto | Se resuelve en | Colección en el paquete `ansible` 12.3.0 |
|---|---|---|
| `docker` | `community.docker.docker` | community.docker 4.8.5 |
| `podman` | `containers.podman.podman` | containers.podman 1.18.0 |
| `buildah` | `containers.podman.buildah` | containers.podman 1.18.0 |
| `kubectl` | `kubernetes.core.kubectl` | kubernetes.core 6.2.0 |
| `lxd` | `community.general.lxd` | community.general 11.4.2 |
| `chroot` | `community.general.chroot` | community.general 11.4.2 |
| `aws_ssm` | `community.aws.aws_ssm`, que `community.aws` 10.0.0 reenvía a `amazon.aws.aws_ssm` | amazon.aws 10.1.2 |
| `incus` | sin nombre corto: escribe `community.general.incus` | community.general 11.4.2 |

Con `-vvv`, un nombre redirigido muestra `redirecting (type: connection) ansible.builtin.chroot to community.general.chroot` bajo la primera tarea que lo usa en un host, una línea por salto.

Un nombre que se resuelve en un plugin que Volant todavía no maneja, como `winrm`, `psrp` o `kubernetes.core.kubectl`, marca el host como inalcanzable con `connection '<full name>' is not supported` y el resto de la ejecución continúa. La [matriz de compatibilidad](/es/docs/reference/parity/#collection-connection) enumera todos los plugins de conexión de las colecciones del paquete.

Un nombre que no se resuelve en nada hace fallar la tarea con `the connection plugin '<name>' was not found`, como hace Ansible. Eso incluye un nombre corto dirigido a una colección que no está instalada, como `docker` sin community.docker, tras el aviso que ansible-core muestra para él, y `paramiko`, que ansible-core 2.19.12 no tiene con ese nombre. Esto vale tanto si el valor es la conexión del propio host, la de un destino de `delegate_to` o la del host que ejecuta una tarea `run_once`.

Una contraseña viene de `-k`, de `--connection-password-file` o de la variable `ansible_password`. Volant se la entrega a `ssh` mediante `SSH_ASKPASS` y un archivo temporal privado que se elimina al final de la ejecución, lo que exige OpenSSH 8.4 o posterior. Entonces `ssh` se ejecuta con `BatchMode=no` y `NumberOfPasswordPrompts=1`, de modo que una respuesta incorrecta termina el intento. La contraseña nunca aparece en una línea de comandos, en el entorno de un programa ni en la salida, ni siquiera con `-vvvv`. Un `ansible_password` cifrado con vault funciona como cualquier variable cifrada. Sin contraseña, `ssh` se ejecuta con `BatchMode=yes` y nunca pregunta, así que un host que pediría una contraseña o una frase de paso se marca como inalcanzable en lugar de bloquear la ejecución.

## Contenedores Docker

`ansible_connection: docker`, o `community.docker.docker`, ejecuta un play dentro de un contenedor en marcha, sin sshd dentro ni camino de red hasta él. Los dos nombres necesitan community.docker instalado en el controlador, como con `ansible-playbook`. Volant ejecuta el programa `docker` de tu `PATH`, así que `DOCKER_HOST`, `DOCKER_CONTEXT` y `DOCKER_CONFIG` le llegan por el entorno.

El agente sube por `docker exec -i`, así que la imagen necesita `/bin/sh` y las herramientas habituales: `uname`, `id`, `ls`, `mkdir`, `df`, `awk`, `head`, `wc`, `chmod`, `mv` y `find`. Una imagen mínima de Alpine o Debian las tiene. Una imagen sin shell, como distroless, recibe el agente con `docker cp`: Volant pregunta a `docker inspect` la arquitectura de la imagen, copia el agente en el `/tmp` del contenedor y el agente se coloca a sí mismo en su directorio de caché, con las mismas comprobaciones. El contenedor necesita un `/tmp`, `remote_tmp` debe ser absoluto o empezar por `~` para un usuario que tenga directorio personal, y `become` se rechaza, porque no hay shell desde la que lanzar `sudo`. La copia en `/tmp` se queda cuando el usuario del contenedor no puede borrarla.

| Variable | Qué hace |
|---|---|
| `ansible_docker_host` | El nombre o el id del contenedor. Gana a `ansible_host`, que gana al nombre del inventario. |
| `ansible_docker_user` | El usuario con el que se ejecutan los comandos, `docker exec -u`. Gana a `ansible_user`, a la palabra clave `remote_user` y a `-u`. Sin definir, el usuario propio del contenedor. |
| `ansible_docker_extra_args` | Argumentos puestos después de `docker` y antes de `exec`, separados por espacios simples y no como palabras de shell. El valor por defecto viene de `[docker_connection] extra_cli_args`. |
| `ansible_docker_extra_env` | Un diccionario de variables que se fijan con `docker exec -e`. Cada valor debe ser texto: un número o un booleano sin comillas se rechaza. |
| `ansible_docker_privileged` | `docker exec --privileged`. El valor por defecto viene de `ANSIBLE_DOCKER_PRIVILEGED` o de `[docker_connection] privileged`. |
| `ansible_docker_working_dir` | `docker exec -w`. El valor por defecto viene de `ANSIBLE_DOCKER_WORKING_DIR` o de `[docker_connection] working_dir`. |
| `ansible_docker_timeout` | Los segundos que `docker` puede tardar en responder al primer comando. El valor por defecto viene de `ANSIBLE_DOCKER_TIMEOUT`, luego de `[docker_connection] timeout` y luego del tiempo de espera de la conexión. |

`become` con `sudo` funciona con el `sudo` del propio contenedor. `su` se rechaza, porque `docker exec` no le da terminal. Un contenedor que no existe o está parado, o un usuario que la imagen no tiene, deja el host inalcanzable con el mensaje propio de docker. ansible-core informa de los tres casos como `Failed to create temporary directory`, así que este mensaje difiere a propósito.

Con `-vvvv` las líneas muestran `ESTABLISH DOCKER CONNECTION FOR USER: <user>` y luego una línea `EXEC [...]` por comando, que son comandos propios de Volant. Cuando el play no nombra usuario, viene de `docker inspect`: `root` cuando la imagen no nombra ninguno, `?` cuando docker no puede decirlo.

## Contenedores Podman

`ansible_connection: podman`, o `containers.podman.podman`, ejecuta un play en un contenedor de Podman, rootless o no, sin demonio ni ssh. Los dos nombres necesitan containers.podman instalado en el controlador. El agente sube como con docker: con `podman exec -i` cuando la imagen tiene shell, con `podman cp` cuando no la tiene.

Volant lee las opciones como las lee el código fuente del plugin, que difiere de su documentación. El contenedor es `ansible_host` o, si no, el nombre del inventario; el `ansible_podman_host` documentado no se lee. El usuario es `ansible_user`, la palabra clave `remote_user` o `-u`, pasado como `podman exec --user`.

| Variable | Qué hace |
|---|---|
| `ansible_podman_executable` | El programa `podman`, un nombre que se busca en tu `PATH` o una ruta. El valor por defecto viene de `ANSIBLE_PODMAN_EXECUTABLE`. |
| `ansible_podman_extra_args` | Argumentos puestos después de `podman` y antes del subcomando, separados como lo haría una shell. El valor por defecto viene de `ANSIBLE_PODMAN_EXTRA_ARGS` o de `[defaults] podman_extra_args`. |

El plugin ejecuta `podman exec` sin `-i`, así que no puede pasar su stdin a un comando. El agente lo necesita, por eso Volant ejecuta `podman exec -i`; es la única palabra que añaden sus líneas de comando. Volant nunca ejecuta `podman mount`. `become` con `sudo` funciona con el `sudo` del propio contenedor, y `su` se rechaza. Con `-vvvv` cada comando aparece como una línea `RUN [...]`, como la escribe el plugin, sin línea `ESTABLISH`.

## Directorios chroot

`ansible_connection: chroot`, o `community.general.chroot`, ejecuta un play dentro de un árbol de directorios del controlador: la construcción de una imagen, un árbol de `debootstrap`, un rescate. El nombre del inventario, o `ansible_host`, es el directorio. Los dos nombres necesitan community.general instalado en el controlador. La ejecución necesita root, porque `chroot` lo necesita.

| Variable | Qué hace |
|---|---|
| `ansible_chroot_exe` | El programa `chroot`, un nombre que se busca en tu `PATH` o una ruta absoluta. El valor por defecto viene de `ANSIBLE_CHROOT_EXE` o de `[chroot_connection] exe`. |
| `ansible_chroot_disable_root_check` | Omite la comprobación de que el controlador se ejecuta como root. El valor por defecto viene de `ANSIBLE_CHROOT_DISABLE_ROOT_CHECK` o de `[chroot_connection] disable_root_check`. |
| `ansible_executable` | La shell con la que se ejecutan los comandos, `/bin/sh` por defecto. El valor por defecto viene de `ANSIBLE_EXECUTABLE` o de `[defaults] executable`. |

Antes de que se ejecute nada, la tarea falla, como con ansible-core, para una ruta que no es un directorio, para un árbol sin `bin/sh` y para un controlador que no es root, salvo que la comprobación esté desactivada. El agente sube por `chroot <dir> /bin/sh`, así que el script que lo escribe se ejecuta dentro del árbol, y ninguna ruta que escriba puede salir de él, tenga los enlaces simbólicos que tenga. Cuando `df` no puede decir cuánto espacio queda, como en un árbol sin `/proc` montado, se omite la comprobación de espacio. `become` funciona cuando el árbol tiene `sudo`. Con `-vvvv` las líneas muestran `THIS IS A LOCAL CHROOT DIR` y luego una línea `EXEC [...]` por comando.

## Variables de host

Volant lee estas variables por host, desde el inventario o desde cualquier otra fuente de variables. Una prueba compara esta lista con los nombres que lee el código de conexión, así que se mantiene sincronizada con el motor.

| Variable | Efecto |
| --- | --- |
| `ansible_connection` | `ssh` (por defecto) o `local` |
| `ansible_host`, `ansible_ssh_host` | Dirección a la que conectar. Por defecto, el nombre del inventario. Un valor que empieza por `-` o contiene espacios o un carácter de control se rechaza antes de ejecutar `ssh`. |
| `ansible_port`, `ansible_ssh_port` | `ssh -p`. Un número entre comillas funciona igual que uno sin ellas. |
| `ansible_user`, `ansible_ssh_user` | `ssh -l` |
| `ansible_password`, `ansible_ssh_pass`, `ansible_ssh_password` | La contraseña de conexión, ofrecida a `ssh` como se describió antes. |
| `ansible_private_key_file`, `ansible_ssh_private_key_file` | `ssh -i` |
| `ansible_timeout`, `ansible_ssh_timeout` | Segundos que `ssh` espera la conexión, `ssh -o ConnectTimeout`. |
| `ansible_ssh_executable` | El programa `ssh` que se ejecuta. Por defecto, `ssh` de tu `PATH`. |
| `ansible_ssh_common_args` | Argumentos adicionales de `ssh`, divididos como los dividiría un shell |
| `ansible_ssh_extra_args` | Lo mismo, añadido después de los comunes |
| `ansible_ssh_use_tty` | Ejecuta los comandos `raw` y `script` en un terminal, como hace `ssh -tt`. Activo salvo que se ponga a false. El valor por defecto viene de [`usetty`](/es/docs/reference/configuration/#ssh-connection-usetty). |
| `ansible_remote_tmp` | Directorio que contiene la caché del agente en ese host |
| `ansible_become` | Escalar privilegios o no, con prioridad sobre la palabra clave `become` |
| `ansible_become_user`, `ansible_sudo_user`, `ansible_su_user` | Usuario de destino del escalado |
| `ansible_become_method` | `sudo` o `su` |
| `ansible_become_password`, `ansible_become_pass`, `ansible_sudo_pass`, `ansible_su_pass` | Contraseña del escalado |
| `ansible_become_exe`, `ansible_sudo_exe`, `ansible_su_exe` | El programa de escalado |
| `ansible_become_flags`, `ansible_sudo_flags`, `ansible_su_flags` | Sus opciones |
| `ansible_su_prompt_l10n` | Una lista de prompts que `su` puede imprimir. Sustituye a los integrados. |

Cuando un host define una variable y un alias suyo, se aplica el que Ansible deja ganar. La tabla los enumera del más fuerte al más débil, con la palabra clave que define lo mismo:

| Ajuste | Variables | Palabra clave |
|---|---|---|
| Dirección | `ansible_ssh_host`, `ansible_host` | |
| Puerto | `ansible_ssh_port`, `ansible_port` | `port` |
| Usuario | `ansible_ssh_user`, `ansible_user` | `remote_user` |
| Archivo de clave | `ansible_ssh_private_key_file`, `ansible_private_key_file` | |
| Tiempo de espera | `ansible_ssh_timeout`, `ansible_timeout` | |
| Contraseña | `ansible_ssh_password`, `ansible_ssh_pass`, `ansible_password` | |
| Conexión | `ansible_connection` | `connection` |
| Usuario, programa y opciones del escalado | `ansible_sudo_*` o `ansible_su_*` (los del propio método), después `ansible_become_*` | `become_user`, `become_exe`, `become_flags` |
| Contraseña del escalado | `ansible_sudo_pass` o `ansible_su_pass`, después `ansible_become_pass`, después `ansible_become_password` | |

Una variable de contraseña gana a `-k` y a `--connection-password-file`, y una variable con una cadena vacía no ofrece ninguna contraseña. Una variable de conexión funciona en todos los niveles que definen variables: inventario, `group_vars`, las `vars` de un play, un bloque, las `vars` de una tarea y `-e`.

Las palabras clave funcionan en un play, un bloque y una tarea, y `-c` fija la conexión de los hosts que no nombran ninguna. El orden es el que usa Ansible: gana una variable de host como `ansible_user`, después la tarea, el bloque y el play, y por último la línea de comandos y `ansible.cfg`. Los facts que un host informa sobre sí mismo nunca eligen su usuario, su puerto ni su conexión. Una palabra clave escrita como plantilla se renderiza una vez por tarea. Estos valores hacen fallar la tarea: un nombre de usuario que empieza por `-` o contiene un espacio, una comilla o un carácter de control, un puerto fuera del rango 1-65535 y un nombre de conexión con algo que no sean letras, dígitos, `_` y `.`. Una tarea delegada usa el `remote_user` y el `port` de la tarea, salvo que el destino defina su propio `ansible_user` o `ansible_port`.

Una comilla sin cerrar en `ansible_ssh_common_args` o en `ansible_ssh_extra_args` hace fallar el host en lugar de descartarse. Importa detrás de un bastión: descartar en silencio un `ProxyCommand` conectaría directamente a una dirección que podría pertenecer a otra máquina.

Los ajustes de `ansible.cfg` que afectan a las conexiones, como `timeout`, `remote_user`, `host_key_checking` y `forks`, están en [Configuración](/es/docs/reference/configuration/).

## La caché del agente

El agente es un programa estático pequeño, de unos 1.2 MB para Linux x86_64. Volant lo sube una vez por host y lo reutiliza en todas las ejecuciones siguientes, así que un host guarda solo el agente y, cuando se ejecutaron módulos de Python, una caché de payloads.

Vive en `<remote_tmp>/volant-agent-<version>-<uid>/volant-agent`, de modo que con el `remote_tmp` por defecto queda bajo `~/.ansible/tmp/` en un directorio personal. La versión de la ruta es la del propio controlador, y así un controlador actualizado evita ejecutar un agente anterior. Una tarea que escala privilegios ejecuta su propio agente como `become_user`, en la caché del directorio personal de ese usuario, de modo que un host guarda una caché por cada cuenta con la que actúa la ejecución.

Cuando se han ejecutado módulos de Python, un segundo directorio, `volant-blobs-<version>-<uid>`, queda junto al directorio del agente en el mismo `remote_tmp` y contiene los payloads de módulos que envió Volant.

Cómo llega el agente depende del destino. Por `ssh`, y en un contenedor que tiene shell, el agente se pasa a un script de shell por la conexión, que comprueba el directorio y escribe el archivo. En un contenedor sin shell, el propio comando de copia del runtime deja el agente dentro, y el agente se coloca a sí mismo con las mismas comprobaciones; docker lo hace con `docker cp`.

Para borrar ambos, elimina los dos directorios. No queda nada más:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-agent-* ~/.ansible/tmp/volant-blobs-*'
```

[El agente en tus hosts](/es/docs/internals/agent/#la-caché-del-agente) describe la subida, la reparación de una caché dañada y las comprobaciones de permisos.

## Conexiones persistentes

Un host mantiene su conexión con el agente durante toda la ejecución, a través de los plays. Las tareas viajan por esa única conexión en lugar de abrir una nueva cada vez. Si la conexión se cae entre dos plays, Volant se reconecta una vez, y un segundo fallo marca el host como inalcanzable.

Algunas tareas añaden una conexión:

- Escalado de privilegios. Un host que usa `become` mantiene una segunda conexión, con un agente que se ejecuta como el usuario de destino. Se conserva hasta el final del play, así que un play de tareas con escalado inicia un solo agente escalado, no uno por tarea. Un host mantiene como máximo una conexión escalada a la vez: escalar a otro usuario la sustituye.
- Delegación. Una tarea delegada se ejecuta por la conexión propia del destino, con los ajustes del destino. Un host cuyo play delega en otros tres hosts mantiene esas tres conexiones junto a la suya.

## Una conexión por host

Todas las ejecuciones de `ssh` que hace Volant para un host del inventario comparten una única conexión OpenSSH, así que solo la primera paga el intercambio de claves y la autenticación. La conexión compartida sigue abierta 30 segundos después de su última sesión. OpenSSH hace el trabajo, de modo que `~/.ssh/config`, `Match`, `Include`, `ProxyJump` y tu ssh-agent se aplican exactamente igual que con `ansible-playbook`.

Volant deja la compartición en tus manos cuando tus propios argumentos de `ssh` o tu configuración ya definen `ControlMaster`, `ControlPath` o `ControlPersist` para el host. Pon `[volant] ssh_control_master = false` o `VOLANT_SSH_CONTROL_MASTER=0` para desactivarla, consulta [Configuración](/es/docs/reference/configuration/#volant-ssh-control-master).

Cada conexión cuesta al controlador tres descriptores de archivo. Un límite estricto de 1024 archivos abiertos detiene una ejecución en unas 330 conexiones, o en unos 169 hosts con escalado. [El agente en tus hosts](/es/docs/internals/agent/#una-conexión-por-host) tiene la ubicación del socket, los ajustes de keepalive y la comprobación que conviene hacer antes de una ejecución amplia.

## Become

`become` ejecuta tareas como otro usuario mediante `sudo` o `su`.

```yaml
- hosts: web
  become: true
  tasks:
    - name: Install nginx
      apt:
        name: nginx
        state: present

    - name: Check the app as its own user
      command: /opt/app/bin/healthcheck
      become_user: app
```

### Cómo funciona

Volant no envuelve cada tarea en `sudo`. Inicia un segundo agente para el usuario de destino:

```sh
sudo -H <form> -u <user> -- volant-agent
```

`<form>` es `-n` o `-k -S -p ''`, según una sonda que se describe más abajo. Volant añade `-H` solo cuando no hay `become_flags`: una lista de opciones lo sustituye. Las tareas que no escalan siguen usando el agente sin privilegios. El agente en sí no sabe nada del escalado. El [registro de decisión 0004](/es/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explica por qué.

El agente escalado se conserva hasta el final del play, así que un play de tareas con escalado paga `sudo` una vez por host.

### Qué ajuste gana

`become`, `become_user` y `become_method` se aceptan en un play, un bloque y una tarea. Medido contra Ansible:

1. Las variables de host `ansible_become` y `ansible_become_user` ganan a ambas palabras clave, en cualquier sentido.
2. La palabra clave de la tarea gana a la del play.
3. Los valores por defecto de `ansible.cfg` y `-b`, `--become-user` y `--become-method` se aplican en último lugar.

`become_method` es `sudo` o `su`. Cualquier otro método todavía no está disponible, y tampoco los nombres completos `ansible.builtin.sudo` y `ansible.builtin.su`. Volant se detiene antes de ejecutar una tarea y nombra el método, en lugar de escalar por un mecanismo que no pediste. Cuando una ejecución escala y el método viene de `--become-method`, de `ansible.cfg` o de `ansible_become_method`, la ejecución termina con código de salida 2 antes de cualquier tarea.

`become_user` puede ser una plantilla como `{{ app_user }}`. Se rechaza una que cambie con cada elemento de un bucle, porque un lote escala a un único usuario.

`become_exe` y `become_flags` se leen de la palabra clave y de `ansible_become_exe` y `ansible_become_flags`, y gana la variable de host. Ambos se dividen en palabras como lo hace un shell, y una comilla sin cerrar hace fallar la tarea.

### La contraseña de become

La contraseña viene de una de estas fuentes:

- `-K` (`--ask-become-pass`), que desactiva el eco del terminal en Unix.
- `--become-password-file`, que lee la primera línea de un archivo.
- `ansible_become_password`, `ansible_become_pass`, o los `ansible_sudo_pass` o `ansible_su_pass` propios del método, que gana a los otros dos. [Variables de host](#variables-de-host) indica qué variable gana.

Se escribe una sola vez en la entrada estándar de `sudo`. Nunca aparece en una línea de comandos, en un log ni en un resultado: los tipos que la llevan imprimen `<redacted>` en la salida de depuración, y la salida de error del propio `sudo` se limpia de ella antes de llegar a un mensaje.

### La sonda de sudo

Volant le pregunta a `sudo` qué forma usar en lugar de suponer una. Prueba primero `sudo -n` y recurre a `sudo -k -S -p ''` solo cuando el propio `sudo` rechaza la petición por falta de autenticación. El [registro de decisión 0004](/es/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) explica por qué una suposición bloquearía el enlace. En un host que ya se usó, Volant arranca primero el agente en caché con `sudo -n` en la misma sesión `ssh` que sus comprobaciones, y solo ejecuta la sonda cuando eso no funciona, como describe [El agente en tus hosts](/es/docs/internals/agent/).

Cuando el escalado falla, por falta de contraseña, por una contraseña incorrecta o por una regla de `sudoers` que prohíbe el comando, la tarea falla y el host sigue siendo alcanzable. Los mensajes son los propios de Ansible: `Missing sudo password` o `Incorrect sudo password`.

### su

`su` no tiene ninguna opción para leer una contraseña sin terminal, así que Volant responde a su prompt. La contraseña se escribe una sola vez en el prompt y nunca aparece en una línea de comandos, en un log ni en la salida de `-vvv`. Una contraseña incorrecta hace fallar la tarea con `Incorrect su password`. Un prompt que nunca llega termina la tarea con `Timeout (12s) waiting for privilege escalation prompt`, como en ansible-core.

En una conexión `local`, `su` se ejecuta en un terminal propio. Una contraseña incorrecta hace fallar la tarea con `Premature end of stream waiting for become success.` y lo que imprimió `su`, como hace ansible-core en ese caso. La espera del prompt es siempre de 12 segundos en una conexión local, diga lo que diga `-T`.

## Todavía no disponible

- Hosts Windows y dispositivos de red.
- Los plugins de conexión distintos de `ssh` y `local`, enumerados en la [matriz de compatibilidad](/es/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` y los demás métodos de become. Se pueden añadir como otras formas de iniciar el agente, sin cambiar el protocolo.

[Palabras clave](/es/docs/reference/keywords/) es la lista completa, generada a partir de las tablas que lee el motor.
