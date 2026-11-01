---
sourceHash: 3b083b74d53c
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
| `buildah` | Ejecuta el agente en un contenedor de trabajo de Buildah mediante `buildah run`, como `containers.podman.buildah`. Consulta [Contenedores de trabajo de Buildah](#contenedores-de-trabajo-de-buildah). |
| `lxd` | Ejecuta el agente en una instancia de LXD mediante `lxc exec`, como `community.general.lxd`; `community.general.incus` hace lo mismo para Incus. Consulta [Instancias de LXD e Incus](#instancias-de-lxd-e-incus). |
| `kubectl` | Ejecuta el agente en un contenedor de un pod de Kubernetes mediante `kubectl exec`, como `kubernetes.core.kubectl`. Consulta [Pods de Kubernetes](#pods-de-kubernetes). |
| `aws_ssm` | Ejecuta el agente en una instancia EC2 mediante una sesión de AWS Systems Manager, como `amazon.aws.aws_ssm`. Consulta [Sesiones de AWS Systems Manager](#sesiones-de-aws-systems-manager). |
| `paramiko_ssh` | Ejecuta el cliente OpenSSH en lugar de la conexión paramiko, obsoleta en ansible-core; `paramiko` es lo mismo. Consulta [Paramiko](#paramiko). |

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

Un nombre que se resuelve en un plugin que Volant no maneja por sí mismo, como `winrm`, `psrp`, `community.general.jail` o un plugin en un directorio `connection_plugins/` junto al playbook, toma la segunda vía: consulta [Conexiones que solo tiene ansible-core](#conexiones-que-solo-tiene-ansible-core). Sin el ansible-core del controlador al que preguntar, ese nombre marca el host como inalcanzable con `connection '<full name>' is not supported` y el resto de la ejecución continúa. La [matriz de compatibilidad](/es/docs/reference/parity/#collection-connection) enumera todos los plugins de conexión de las colecciones del paquete.

Un nombre que no se resuelve en nada hace fallar la tarea con `the connection plugin '<name>' was not found`, como hace Ansible. Eso incluye un nombre corto dirigido a una colección que no está instalada, como `docker` sin community.docker, tras el aviso que ansible-core muestra para él, y `ansible.builtin.paramiko`, que ansible-core 2.19.12 no tiene con ese nombre. Esto vale tanto si el valor es la conexión del propio host, la de un destino de `delegate_to` o la del host que ejecuta una tarea `run_once`.

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

## Contenedores de trabajo de Buildah

`ansible_connection: buildah`, o `containers.podman.buildah`, configura el contenedor de trabajo que crea `buildah from <imagen>`, para que un play construya una imagen capa a capa, que luego guarda `buildah commit`. Los dos nombres necesitan containers.podman instalado en el controlador. Como con podman, el contenedor es `ansible_host` o, si no, el nombre del inventario, y el usuario es `ansible_user`, la palabra clave `remote_user` o `-u`; `root` no añade ninguna opción. El programa es el `buildah` de tu `PATH`, y las variables `BUILDAH_*` le llegan por el entorno.

El agente arranca con `buildah run [--user U] <contenedor> -- <agente>`. Volant nunca ejecuta `buildah mount`, que necesita `buildah unshare` para un usuario rootless: un contenedor de trabajo sin shell recibe el agente con `buildah copy`. buildah en rootless escribe avisos sobre capacidades en cada comando, así que el stderr del agente no se muestra. Con `-vvvv` cada comando aparece como una línea `RUN [...]`, sin línea `ESTABLISH`.

## Instancias de LXD e Incus

`ansible_connection: lxd`, o `community.general.lxd`, ejecuta un play en una instancia de LXD, local o en un servidor remoto, mediante el comando `lxc`. `community.general.incus` hace lo mismo para Incus con el comando `incus`; no tiene nombre corto en ansible-core. Los dos necesitan community.general instalado en el controlador.

| Variable | Qué hace |
|---|---|
| `ansible_lxd_host`, `ansible_incus_host` | La instancia, hasta el primer punto: `web.example.com` es `web`. Gana a `ansible_host`, que gana al nombre del inventario. |
| `ansible_lxd_remote`, `ansible_incus_remote` | El servidor remoto, `local` por defecto. |
| `ansible_lxd_project`, `ansible_incus_project` | El proyecto. Incus siempre pasa uno, `default` por defecto. |
| `ansible_lxd_executable`, `ansible_incus_executable` | La shell con la que se ejecutan los comandos. Gana a `ansible_executable`; `/bin/sh` por defecto. |

El usuario es `ansible_user`, la palabra clave `remote_user` o `-u`, `root` por defecto. Para cualquier otro usuario, cada comando pasa por el método de escalada, `/bin/su <user> -c` salvo que `lxd_become_method` o `incus_become_method` nombre otro. Una instancia sin shell recibe el agente con `lxc file push` o `incus file push` cuando el usuario es root. Una instancia que no existe o está parada deja el host inalcanzable con las palabras del plugin, como `instance not running: web`; Incus añade el servidor remoto y el proyecto. Con `-vvvv` las líneas muestran `ESTABLISH LXD CONNECTION FOR USER: <user>` (`Incus` para Incus), la línea `INFO:` del plugin para un usuario que no es root y luego una línea `EXEC [...]` por comando.

## Pods de Kubernetes

`ansible_connection: kubectl`, o `kubernetes.core.kubectl`, ejecuta un play en un contenedor de un pod mediante `kubectl exec -i`. Los dos nombres necesitan kubernetes.core instalado en el controlador y `kubectl` en tu `PATH`.

| Variable | Qué hace |
|---|---|
| `ansible_kubectl_pod` | El pod. Sin ella, el pod es `ansible_host` o, si no, el nombre del inventario. |
| `ansible_kubectl_container` | El contenedor, para un pod que tiene más de uno. |
| `ansible_kubectl_namespace` | El namespace. |
| `ansible_kubectl_kubeconfig`, `ansible_kubectl_config` | El archivo kubeconfig. Un kubeconfig dado como mapeo va a un archivo temporal privado, que se borra al final de la ejecución. |
| `ansible_kubectl_extra_args` | Más argumentos para `kubectl`, separados por espacios. |
| `ansible_kubectl_local_env_vars` | Un mapeo de variables que se ponen en el entorno de cada comando `kubectl`. |
| `ansible_kubectl_context` | El contexto dentro del kubeconfig. |
| `ansible_kubectl_server`, `ansible_kubectl_host` | El servidor de la API. |
| `ansible_kubectl_username`, `ansible_kubectl_user`, `ansible_kubectl_password` | Un usuario y una contraseña para el servidor de la API. |
| `ansible_kubectl_token`, `ansible_kubectl_api_key` | Un token bearer para el servidor de la API. |
| `ansible_kubectl_client_cert`, `ansible_kubectl_cert_file`, `ansible_kubectl_client_key`, `ansible_kubectl_key_file` | El certificado de cliente y su clave. |
| `ansible_kubectl_ca_cert`, `ansible_kubectl_ssl_ca_cert` | La autoridad de certificación del servidor de la API. |
| `ansible_kubectl_validate_certs`, `ansible_kubectl_verify_ssl` | Se pasa tal cual tras `--insecure-skip-tls-verify`, como hace el plugin: `true` da `True`, y `false` no añade nada. |

Cada opción también puede venir de la variable de entorno `K8S_AUTH_*` que lee el plugin, a la que gana una variable de host. Las opciones llegan a `kubectl` en el orden del plugin. Los comandos se ejecutan con el usuario del contenedor: el plugin no puede elegir uno, así que `ansible_user` se ignora. Un contenedor sin shell no puede recibir el agente, porque `kubectl cp` necesita `tar` en él. Un pod que no existe, un namespace que no existe y un pod terminado dejan el host inalcanzable con lo que dijo `kubectl`. Con `-vvvv` las líneas muestran `ESTABLISH kubectl CONNECTION` y luego una línea `EXEC [...]` por comando, con la contraseña y el token como `********`.

## Sesiones de AWS Systems Manager

`ansible_connection: amazon.aws.aws_ssm` ejecuta un play en una instancia EC2 mediante AWS Systems Manager Session Manager, sin puerto de entrada, par de claves ni bastión. El nombre corto `aws_ssm` llega a él a través de community.aws, así que también hace falta esa colección. El controlador necesita `session-manager-plugin` y boto3 en su Python, y la instancia el agente de SSM.

| Variable | Qué hace |
|---|---|
| `ansible_aws_ssm_instance_id` | La instancia. Sin ella, la instancia es `ansible_host` o, si no, el nombre del inventario. |
| `ansible_aws_ssm_region` | La región; si no, `AWS_DEFAULT_REGION` o `AWS_REGION`; `us-east-1` por defecto. |
| `ansible_aws_ssm_profile` | El perfil de AWS; si no, `AWS_PROFILE`. |
| `ansible_aws_ssm_access_key_id`, `ansible_aws_ssm_secret_access_key`, `ansible_aws_ssm_session_token` | Las credenciales; si no, las variables de entorno `AWS_*` del mismo nombre y, si no, lo que encuentre boto3. Nunca se muestran, tampoco con `-vvvv`. |
| `ansible_aws_ssm_plugin` | El programa `session-manager-plugin`; si no, `AWS_SESSION_MANAGER_PLUGIN`, `/usr/local/bin/session-manager-plugin` o el de tu `PATH`. |
| `ansible_aws_ssm_document` | El documento de SSM que inicia la sesión; la shell por defecto de la cuenta si no se define. |
| `ansible_aws_ssm_timeout` | Cuántos segundos tiene la sesión para responder en cada paso de la conexión; 60 por defecto. |

Los comandos se ejecutan con el usuario de la sesión, `ssm-user` en Amazon Linux. `become` elige otro, y `ansible_user` se ignora, como con el plugin. Volant no envía nada por S3, así que las variables `ansible_aws_ssm_bucket_*` que necesita el plugin no se leen. El terminal de la sesión solo devuelve UTF-8 válido, así que el agente habla con el controlador en líneas base64, a unos 400 KiB/s en cada sentido, y la primera ejecución en una instancia sube el agente una vez, en unos diez segundos. Una instancia que el agente de SSM no gestiona, o unas credenciales rechazadas, dejan el host inalcanzable con las palabras de boto3, donde el plugin hace fallar la tarea. Cuando termina la ejecución, o se mata el controlador, la sesión se cierra y el agente se detiene con ella. Con `-vvvv` las líneas muestran `ESTABLISH SSM CONNECTION TO: <instance>` y luego una línea `EXEC: <comando>` por comando.

## Paramiko

`ansible_connection: paramiko_ssh` es la conexión de ansible-core mediante la biblioteca de Python paramiko. ansible-core 2.19.12 la marca como obsoleta y la elimina en 2.21. Volant llega a estos hosts con el programa `ssh`, así que no hace falta instalar paramiko. Cada opción del plugin pasa a ser la opción de `ssh` más cercana, y el primer host que se conecta muestra el aviso de ansible-core una vez por ejecución:

```text
[DEPRECATION WARNING]: The paramiko connection plugin is deprecated. This feature will be removed from ansible-core version 2.21.
```

`deprecation_warnings = False` lo silencia. `paramiko`, `_paramiko_ssh` y `ansible.builtin.paramiko_ssh` son el mismo plugin.

| Variable | Qué hace |
|---|---|
| `ansible_paramiko_host` | La dirección. Gana a `ansible_ssh_host`, que gana a `ansible_host`, y después al nombre del inventario. |
| `ansible_paramiko_port` | `ssh -p`. Gana a `ansible_ssh_port` y `ansible_port`, y después a la palabra clave `port`, `ANSIBLE_REMOTE_PARAMIKO_PORT`, `ANSIBLE_REMOTE_PORT`, `[paramiko_connection] remote_port` y `[defaults] remote_port`. |
| `ansible_paramiko_user` | `ssh -l`. Gana a `ansible_ssh_user` y `ansible_user`, y después a la palabra clave `remote_user`, `ANSIBLE_PARAMIKO_REMOTE_USER`, `[paramiko_connection] remote_user` y los ajustes habituales del usuario remoto. |
| `ansible_paramiko_private_key_file` | `ssh -i`. Gana a `ansible_ssh_private_key_file` y `ansible_private_key_file`, y después a `ANSIBLE_PARAMIKO_PRIVATE_KEY_FILE`, `[paramiko_connection] private_key_file` y los ajustes habituales de la clave. |
| `ansible_paramiko_password`, `ansible_paramiko_pass` | La contraseña, que `ssh` recibe como para un host `ssh`. Ganan a `ansible_ssh_password`, `ansible_ssh_pass` y `ansible_password`. |
| `ansible_paramiko_timeout` | `ConnectTimeout`. Gana a `ansible_ssh_timeout`, y después a `ANSIBLE_PARAMIKO_TIMEOUT`, `ANSIBLE_SSH_TIMEOUT`, `[paramiko_connection] timeout`, `[ssh_connection] timeout` y el tiempo de espera de la conexión. `ansible_timeout` no se lee, como en el plugin. |
| `ansible_paramiko_host_key_checking`, `ansible_ssh_host_key_checking`, `ansible_host_key_checking` | Si se comprueba la clave del host, en ese orden, y después `ANSIBLE_PARAMIKO_HOST_KEY_CHECKING`, `ANSIBLE_SSH_HOST_KEY_CHECKING`, `ANSIBLE_HOST_KEY_CHECKING`, `[paramiko_connection] host_key_checking` y `[defaults] host_key_checking`. Desactivada, `ssh` recibe `StrictHostKeyChecking=no` y `UserKnownHostsFile=/dev/null`. |
| `ansible_paramiko_use_rsa_sha2_algorithms` | Desactivada, `ssh` recibe `HostKeyAlgorithms=+ssh-rsa` y `PubkeyAcceptedAlgorithms=+ssh-rsa`, para un servidor que solo conoce las claves RSA como `ssh-rsa`. Con un servidor que ofrece firmas SHA-2, `ssh` sigue prefiriéndolas. También `ANSIBLE_PARAMIKO_USE_RSA_SHA2_ALGORITHMS` y `[paramiko_connection] use_rsa_sha2_algorithms`. |
| `ansible_paramiko_proxy_command` | `ProxyCommand`, donde `ssh` sustituye `%h`, `%p` y `%r` como paramiko. También `ANSIBLE_PARAMIKO_PROXY_COMMAND` y `[paramiko_connection] proxy_command`. |

Las opciones sin variable vienen de `ANSIBLE_PARAMIKO_<NAME>` o de `[paramiko_connection]`:

| Opción | Qué recibe `ssh` |
|---|---|
| `host_key_auto_add` | Activada, con la comprobación de claves activada: `StrictHostKeyChecking=accept-new`. Se acepta la clave de un host nuevo, y una clave cambiada se sigue rechazando. |
| `record_host_keys` | Desactivada, con la comprobación de claves activada: `UserKnownHostsFile=/dev/null ~/.ssh/known_hosts ~/.ssh/known_hosts2`. `ssh` comprueba contra los archivos habituales y escribe una clave nueva en el primero, que no guarda nada. |
| `look_for_keys` | Desactivada, con un archivo de clave indicado: `IdentitiesOnly=yes`, así que solo se prueba esa clave. Sin archivo de clave no cambia nada. |
| `pty` | Desactivada: sin terminal, como hace `ansible_ssh_use_tty: false` para `ssh`. |
| `banner_timeout` | Se lee y se comprueba, sin efecto: `ssh` no tiene ese ajuste. |

En qué se diferencia del plugin:

- No se leen `~/.ssh/config` ni `/etc/ssh/ssh_config`, como en paramiko: cada `ssh` recibe `-F /dev/null`. Tampoco se leen `ansible_ssh_common_args`, `ansible_ssh_extra_args` ni `ansible_ssh_executable`.
- Una clave de host desconocida, con la comprobación activada y `host_key_auto_add` desactivada, deja el host inalcanzable con el mensaje de `ssh`. El plugin pregunta en el terminal.
- Los mensajes de error son los de `ssh`, tanto para una clave de host desconocida como para un inicio de sesión rechazado, y el host queda inalcanzable en los dos casos, como con el plugin.
- Con `look_for_keys` desactivada y un archivo de clave indicado, tampoco se ofrecen las claves de `ssh-agent`. paramiko sí las ofrece.
- La conexión a un host se comparte entre tareas, como con `ssh`.

Con `-vvvv`, la primera línea es `ESTABLISH PARAMIKO SSH CONNECTION FOR USER: <user> on PORT <port> TO <address>`, seguida de las líneas `SSH: EXEC` de un host `ssh`.

## Conexiones que solo tiene ansible-core

Volant llega a un host por una de dos vías. Para `ssh`, `local` y las conexiones de la lista anterior, ejecuta su agente en el host y le envía las tareas. Para cualquier otro plugin de conexión que el ansible-core del controlador pueda cargar, no hay agente: cada tarea que necesita el host va al ayudante Python de Volant, que carga el plugin de conexión, se conecta, ejecuta por ella el plugin de acción de la tarea como lo haría `ansible-playbook` y vuelve a cerrar la conexión. ansible-core lee cada opción del plugin de las variables del host, de `ansible.cfg` y del entorno, como siempre.

Todo lo demás de la ejecución no cambia: bucles, `when`, `register`, `changed_when`, `failed_when`, `until` y `retries`, `ignore_errors`, `no_log`, `serial`, `delegate_to`, handlers, `async` con `poll: 0` y `async_status`, el modo de comprobación y el recap. Una tarea que se ejecuta en el controlador, como `debug` o `set_fact`, se ejecuta allí como antes.

- Un plugin que no puede conectarse deja el host inalcanzable con el mensaje del propio plugin.
- Las líneas que el plugin imprime con `-vvv`, como `<host> ESTABLISH ... CONNECTION`, se imprimen bajo la tarea con la misma verbosidad.
- `ansible_password`, las demás variables secretas del plugin y la contraseña de become se escriben `********` en cada línea y cada resultado.
- Nunca se ejecutan a la vez más procesos ayudantes de los que permite `forks`.
- `become` usa el método de escalada de la tarea, `sudo` o `su`, lo pida la tarea, su bloque o su play. Escrito como plantilla, por ejemplo `become: "{{ item }}"`, se renderiza para cada elemento del bucle.

Cada tarea paga el arranque de la conexión del plugin, como en `ansible-playbook`, así que esta vía es más lenta por tarea que la del agente.

### Conexiones que mantienen un proceso

`network_cli`, `netconf` y `httpapi` mantienen su conexión con el dispositivo en un proceso propio, como bajo `ansible-playbook`. Antes de la primera tarea que necesita el dispositivo, el helper ejecuta el stub `ansible-connection` de ansible-core con el Python, las rutas de plugins y la verbosidad del helper. El stub arranca el proceso, que escucha en un socket bajo `~/.ansible/pc`, fijado por `ANSIBLE_PERSISTENT_CONTROL_PATH_DIR` o `[persistent_connection] control_path_dir`. El nombre del socket es un hash de la dirección, el puerto, el usuario y la ejecución, así que cada tarea posterior para el mismo dispositivo reutiliza el proceso, desde cualquier helper.

- Un proceso que no puede arrancar hace fallar la tarea con su error, no el host. `-vvv` añade su traceback.
- `meta: reset_connection` termina el proceso del host, y el final de cada play termina todos los procesos que arrancaron sus tareas, como hace `ansible-playbook`. Un proceso inactivo durante `persistent_connect_timeout` segundos (30 por defecto) termina solo, y un comando espera como mucho `persistent_command_timeout` segundos (30).
- Con `-vvvv`, las líneas que ansible-core imprime al arrancar el proceso, como `<address> attempting to start connection` y `<address> local domain socket path is ...`, aparecen bajo la tarea.
- `persistent_log_messages` registra cada intercambio con el dispositivo, contraseñas incluidas, y ansible-core lo advierte con su propio texto.

## Dispositivos de red

`ansible_connection: network_cli`, `netconf` o `httpapi`, o sus nombres completos bajo `ansible.netcommon`, llegan a un switch, un router o un cortafuegos con los propios plugins de ansible.netcommon, que ansible-core carga en el helper de Python de Volant. El controlador necesita ansible.netcommon y ansible.utils instalados, y la colección de la plataforma del dispositivo, como `cisco.ios`. `ansible_network_os` nombra esa plataforma (`cisco.ios.ios`). Un host `network_cli` sin ella queda inalcanzable con `Unable to automatically determine host network os`.

Los módulos de las colecciones de red, como `ansible.netcommon.cli_command` o `cisco.ios.ios_config`, corren en el controlador como bajo `ansible-playbook` y llegan al dispositivo por el proceso persistente de la conexión. Un módulo que necesita otra conexión distinta de la del host falla con el mensaje de su colección, como `Connection type local is not valid for this module`.

Cada conexión necesita paquetes de Python que ansible-core no trae: paramiko o ansible-pylibssh para `network_cli`, ncclient para `netconf`. Con el runtime gestionado de Volant, `volant runtime install --extras network` los instala, y una ejecución que los necesita se detiene antes de la primera conexión mientras falten. Con `VOLANT_PYTHON`, instálalos en ese entorno.

Los plugins leen sus propias opciones de las variables del host, `ansible.cfg` y el entorno, como las lista `ansible-doc -t connection ansible.netcommon.network_cli`.

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

Los hosts a los que se llega por `local` comparten en cambio sus agentes. Cada uno de esos agentes se ejecuta en el controlador con el mismo usuario, así que un host que espera a los demás pasa su agente al siguiente host que se ejecuta, y la ejecución guarda como mucho `forks` agentes inactivos. Un play sobre 500 hosts locales mantiene más o menos tantos agentes como permite `forks`, no 500. Un agente local escalado se queda con su host.

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

Cuando el escalado falla, por falta de contraseña, por una contraseña incorrecta o por una regla de `sudoers` que prohíbe el comando, la tarea falla y el host sigue siendo alcanzable. Los mensajes son los propios de Ansible: `Missing sudo password` o `Incorrect sudo password`. Una tarea con otra contraseña recibe un agente propio, así que una contraseña incorrecta falla aunque la correcta ya haya funcionado, e `ignore_errors` cubre ese fallo como cualquier otro.

### su

`su` no tiene ninguna opción para leer una contraseña sin terminal, así que Volant responde a su prompt. La contraseña se escribe una sola vez en el prompt y nunca aparece en una línea de comandos, en un log ni en la salida de `-vvv`. Una contraseña incorrecta hace fallar la tarea con `Incorrect su password`. Un prompt que nunca llega termina la tarea con `Timeout (12s) waiting for privilege escalation prompt`, como en ansible-core.

En una conexión `local`, `su` se ejecuta en un terminal propio. Una contraseña incorrecta hace fallar la tarea con `Premature end of stream waiting for become success.` y lo que imprimió `su`, como hace ansible-core en ese caso. La espera del prompt es siempre de 12 segundos en una conexión local, diga lo que diga `-T`.

## Todavía no disponible

- Hosts Windows.
- Los plugins de conexión distintos de `ssh` y `local`, enumerados en la [matriz de compatibilidad](/es/docs/reference/parity/#collection-connection).
- `doas`, `pbrun` y los demás métodos de become. Se pueden añadir como otras formas de iniciar el agente, sin cambiar el protocolo.

[Palabras clave](/es/docs/reference/keywords/) es la lista completa, generada a partir de las tablas que lee el motor.
