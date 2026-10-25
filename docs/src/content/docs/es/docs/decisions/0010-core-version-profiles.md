---
sourceHash: c475b50fe6dd
title: "Elegir la versión de ansible-core por proyecto y conservar lo que Ansible retira"
description: "Registro de decisión 0010."
sidebar:
  label: "0010 · Perfiles de versión"
  order: 10
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-10-25 |

## Contexto

Volant reproduce ansible-core 2.19.12 ([decisión 0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/)). Ansible sigue avanzando: 2.20 y 2.21 ya salieron y 2.22 está en release candidate. Cada versión retira o cambia comportamientos de los que dependen los playbooks. Algunos equipos se quedan en la versión que usan hoy, otros quieren el comportamiento más reciente, y una sola referencia no puede servir a los dos.

El 2026-10-25 se volvió a grabar el corpus golden con ansible-core 2.20.10, 2.21.5 y 2.22.0rc1, y se comparó con las grabaciones de 2.19.12 tras quitar las horas, los identificadores de job y los identificadores de proceso. El corpus cubre templates, inventarios y 355 escenarios ejecutados 1032 veces. Catorce archivos difieren con 2.20, 76 con 2.21 y 94 con 2.22. La mayoría de los cambios son pequeños, como una clave añadida a un resultado o un prefijo nuevo en un error, pero algunos cambian una ejecución: un resultado de bucle ya no tiene `skipped`, así que un playbook que lee `r.skipped` se detiene con el código de salida 2 en 2.21. El [anexo](#anexo-diff-del-corpus-2026-10-25) los enumera.

## Decisión

### Los perfiles

Un perfil es la versión de ansible-core cuyo comportamiento reproduce Volant. Hay dos: `2.19`, el predeterminado, que sigue siendo lo que Volant hace hoy, y `2.22`, que se añade cuando salga ansible-core 2.22.0 y se grabe su corpus. `latest` designa el perfil más reciente que trae Volant.

A partir de 2.22 se añade un perfil por cada versión de ansible-core, en la versión de Volant que publica sus grabaciones, y `latest` pasa a ese perfil. Un perfil se retira cuando ansible-core deja de mantener su versión. Las versiones entre 2.19 y 2.22 no tienen perfil propio: sus cambios forman parte de `2.22`.

Un proyecto elige su perfil con `[volant] core_version` en su `ansible.cfg` o con la variable de entorno `VOLANT_CORE_VERSION`, que tiene prioridad sobre el archivo. El valor es una versión (`2.19`, `2.22`) o `latest`. Cualquier otro valor detiene la ejecución con el código de salida 5 y enumera los perfiles. Cómo registra `volant.lock` el perfil se decide junto con el propio lock, en #653.

`ansible_version` indica la versión del perfil: 2.19.12 con `2.19`, y con `2.22` la versión de parche con la que se grabó el corpus de `2.22`. Esto amplía la [decisión 0003](/es/docs/decisions/0003-ansible-version-reports-the-reference-release/). `volant_version` no cambia.

Los módulos Python vienen del ansible-core instalado en el controlador ([decisión 0006](/es/docs/decisions/0006-warm-python-path/)), así que sus resultados siguen esa versión sea cual sea el perfil. Volant avisa al empezar una ejecución cuando la versión instalada no coincide con el perfil.

### Interruptores por comportamiento

Hay un solo motor. Cada diferencia entre versiones es un interruptor con nombre y con la versión en la que empieza, por ejemplo `loop-results-omit-skipped` desde 2.21, y el código pregunta al perfil si el interruptor está activo. Cada interruptor tiene una entrada con el mismo nombre en el registro de abajo.

Se descartaron los conjuntos de comportamiento separados: las diferencias medidas son pequeñas y están repartidas por el motor, así que una copia por versión duplicaría casi todo para cambiar una clave aquí y un mensaje allá.

El código de los módulos y de los action plugins nunca se bifurca. Un módulo nativo o un action plugin tiene una sola implementación, y una diferencia entre versiones es un interruptor dentro de ella. Un interruptor se prueba con un escenario grabado en cada versión que cambia, y la prueba golden ejecuta cada escenario con cada perfil contra la grabación de ese perfil.

### El corpus por perfil

`just golden` recibe la versión como argumento y graba con un ansible-core de esa versión, instalado en un entorno `uv` propio. Las grabaciones de `2.19` se quedan donde están. Otro perfil escribe `expected.<versión>.json` junto a `expected.json`, solo para los escenarios cuyo resultado difiere: 94 de 355 con 2.22.

El Python del controlador se fija junto con la versión. La medición mostró que también cambia los resultados: con los mismos templates de 2.19, Python 3.12 y 3.13 responden distinto a `'abcabc'.replace('b', 'X', count=1)`, a argumentos erróneos en métodos de cadena y a ordenar una lista que contiene `nan`.

Grabar lleva unos 52 minutos por versión para los goldens de templates, inventarios y escenarios en una máquina de 4 núcleos. Lo hace una vez un mantenedor, como hoy, y la CI nunca graba. La CI compara Volant con las grabaciones del repositorio, lo que cuesta una ejecución más de las pruebas golden por perfil.

Un comportamiento entra en un perfil solo con una grabación de esa versión.

### El registro

El registro enumera lo que Ansible retiró o cambió, en `crates/volant/src/profile/registry.toml`. Cada entrada tiene un `name` (el interruptor), la funcionalidad (`feature`), la versión en que se retiró o cambió (`removed_in`), su tipo (`kind`, `security` o `compat`), el reemplazo (`replacement`), el aviso (`warning`) y, para un bug conocido aguas arriba, el comportamiento que sigue Volant (`follow`, `release` o `documented`).

```toml
[[change]]
name = "debug-shows-registered-secret"
feature = "showing a registered secret with `debug`"
removed_in = "2.22"
kind = "security"
replacement = "write the value to a file with `copy` and `no_log: true`"
warning = "showing a registered secret with debug was removed in ansible-core 2.22"

[[change]]
name = "interpreter-auto-legacy"
feature = "`interpreter_python = auto_legacy` and `auto_legacy_silent`"
removed_in = "2.21"
kind = "compat"
replacement = "`auto` or `auto_silent`"
warning = "interpreter_python = auto_legacy was removed in ansible-core 2.21"

[[change]]
name = "loop-results-omit-skipped"
feature = "`skipped: false` in each loop result"
removed_in = "2.21"
kind = "compat"
replacement = "`r.skipped | default(false)`"
warning = "loop results lost their skipped key in ansible-core 2.21"
```

Con un perfil igual o posterior a `removed_in`, una funcionalidad `compat` sigue funcionando, con el aviso seguido de «Volant still runs it» y del reemplazo. Una funcionalidad `security` queda desactivada salvo con `[volant] allow_insecure_legacy = true`. Con un perfil anterior, la funcionalidad simplemente está, sin aviso.

Una entrada es `security` cuando las notas de la versión relacionan el cambio con una CVE, cuando el comportamiento retirado puede mostrar un secreto, o cuando permite al contenido de un playbook leer o escribir fuera de lo que se le dio. Una corrección de seguridad que ansible-core publica en una versión de parche de 2.19 posterior a 2.19.12 también se aplica al perfil `2.19`. Las entradas `security` conocidas son:

- `debug` mostrando un secreto registrado, y las opciones `no_log` sustituidas por un texto de reemplazo en los resultados, ambos cambiados en 2.22;
- el enmascarado de secretos con `$REDACTED$` en toda la salida y en `log_path`, 2.22;
- `tempfile` aceptando una ruta en `prefix` y `suffix`, retirado en 2.19.13, 2.20.9 y 2.21.4;
- las credenciales mostradas en las URL por `uri`, `get_url`, `apt_key`, `rpm_key` y el lookup `url`, enmascaradas desde 2.19.13, 2.20.9 y 2.21.4;
- el directorio persistente de ansible-connection sin permisos privados, corregido en 2.19.14, 2.20.10 y 2.21.5;
- `ansible-galaxy collection install` aceptando entradas de `FILES.json` fuera de la colección (CVE-2026-89091), 2.22.0rc1.

Las demás retiradas de las guías de migración son `compat`: entre ellas `DEFAULT_TRANSPORT=smart` y el parámetro `vaultid` de los filtros `vault` (2.20), el connection plugin `paramiko` y el alias `gpg_whitelist` de `git` (2.21), y `yes` y `no` para `follow_redirects` (2.22).

### `volant upgrade-plan`

```sh
volant upgrade-plan [PATH...] [--to PROFILE] [--json]
```

El comando lee los playbooks, roles, inventarios y el `ansible.cfg` bajo `PATH` (el directorio actual por defecto) y enumera cada uso de una funcionalidad que el perfil de destino (`latest` por defecto) retira o cambia respecto al perfil vigente. Cada hallazgo indica el `archivo:línea`, la entrada del registro y el cambio sugerido.

Algunos cambios no se encuentran leyendo, como un resultado leído mediante un nombre calculado o el texto de un error. El informe termina con las entradas del registro entre los dos perfiles que no puede comprobar. Para esas, un proyecto graba una ejecución con su versión actual y la reproduce con el perfil de destino mediante [`volant shadow replay`](/es/docs/start/switch-without-risk/). Las divergencias son los cambios que encontraría la ejecución.

Los códigos de salida son 0 cuando no se encontró nada, 1 cuando se encontró al menos un uso, 4 cuando no se puede leer un archivo y 5 para un perfil desconocido. `--json` da un objeto por hallazgo: `path`, `line`, `name`, `removed_in`, `kind` y `suggestion`.

### Regresiones conocidas

Volant sigue la versión cuando un cambio es intencionado. Sigue el comportamiento documentado cuando el de la versión es un bug abierto aguas arriba que ejecuta lo que no debe en un host. La respuesta se decide caso por caso y va en el campo `follow` del registro. Para las regresiones conocidas de 2.21:

- el descubrimiento de intérprete que reutiliza la ruta de intérprete de otro host ([ansible/ansible#87288](https://github.com/ansible/ansible/issues/87288)): `documented`, cada host tiene su propio descubrimiento;
- el aviso de descubrimiento que nombra el host del inventario en lugar del host delegado ([ansible/ansible#87298](https://github.com/ansible/ansible/issues/87298)): `documented`;
- el descubrimiento sin caché para los hosts delegados ([ansible/ansible#86517](https://github.com/ansible/ansible/issues/86517)): `documented`, ya que solo cambia el tiempo;
- `dnf` fallando cuando un plugin de dnf escribe en stdout ([ansible/ansible#87366](https://github.com/ansible/ansible/issues/87366)): `release`. El módulo se ejecuta desde el ansible-core del controlador y Volant no parchea módulos. No se encontró ningún cambio en la salida de un `dnf` correcto ni en el changelog ni en el gestor de issues.

### El lock

`volant.lock` fija el motor que ejecuta un proyecto. Si registra el perfil, y qué hace una ejecución cuando el lock y `core_version` no coinciden, se decide en #653.

## Consecuencias

- Un proyecto que no configura nada conserva el comportamiento de `2.19`.
- Un proyecto que pasa a `latest` conserva sus funcionalidades retiradas, con un aviso, salvo las de seguridad.
- Cada diferencia cuesta un interruptor, una entrada en el registro y una grabación antes de entrar en un perfil.
- La implementación sigue en estas issues, por tamaño:
  - el ajuste del perfil, `VOLANT_CORE_VERSION`, `ansible_version` por perfil y la comprobación del ansible-core del controlador (mediana);
  - la grabación por perfil: el argumento de versión de `just golden`, `expected.<versión>.json` y las pruebas golden por perfil (mediana);
  - el registro, sus avisos y `allow_insecure_legacy` (mediana);
  - los interruptores del perfil `2.22`, según el anexo de abajo (grande, dividida por áreas cuando el ajuste esté listo);
  - `volant upgrade-plan` (mediana).

## Anexo: diff del corpus, 2026-10-25

ansible-core 2.20.10, 2.21.5 y 2.22.0rc1 frente a 2.19.12. Las tres primeras se ejecutaron con Python 3.12, 2.22.0rc1 con Python 3.13. Quedan fuera: el orden de los hosts con `order: shuffle` y con `throttle`, que varía de una ejecución a otra; las rutas de los archivos de job y los identificadores de proceso; el propio `ansible_version`; y las diferencias de templates que venían de Python y no de ansible-core.

| Funcionalidad | 2.19.12 | 2.20 | 2.21 | 2.22 | Tipo |
|---|---|---|---|---|---|
| Test `regex` con un `match_type` desconocido | `'str' object is not callable` | `Invalid match_type specified` | como 2.20 | como 2.20 | template |
| `password_hash` con una sal dada | usa la sal (`$1$saltsalt$…`) | como 2.19 | otra sal (`$1$n34PoBLM$…`) | como 2.21 | template |
| Error dentro de un `when` | `Error while evaluating conditional: …` | como 2.19 | `A 'when' expression failed: …` | como 2.21 | template |
| `msg` de un comando que falla | `non-zero return code` | como 2.19 | `The command exited with a non-zero return code.` | como 2.21 | módulo |
| Errores de `include_vars` | ruta sin comillas | ruta entre comillas | como 2.20 | como 2.20 | módulo |
| Resultado de `copy` en check mode | sin clave `file` | clave `file` | como 2.20 | como 2.20 | módulo |
| `command` cuyo `chdir` falla | `rc: null` | como 2.19 | sin clave `rc` | como 2.21 | módulo |
| `cmd` de un comando no encontrado | una cadena | como 2.19 | como 2.19 | una lista | módulo |
| `ansible_parsed` en los resultados de módulo | ausente | ausente | ausente | `true` | módulo |
| `skipped: false` en los resultados de bucle | presente | presente | ausente | ausente | escenario |
| `changed_when_result` | ausente | ausente | presente | presente | escenario |
| Mensaje de `failed_when` | `Action failed: Unknown error.` o el nombre de la acción | como 2.19 | `A 'failed_when' expression evaluated to 'True'.` | como 2.21 | escenario |
| Línea de reintento de `until` y `attempts` | `(1 retries left)`, `attempts: 1` | como 2.19 | `(0 retries left)`, `attempts: 2` | como 2.21 | escenario |
| Elementos de bucle que leen un fact fijado en el mismo bucle | `a-1`, `b-2`, `c-3` | como 2.19 | `a-0`, `b-1`, `c-2` | como 2.21 | escenario |
| `any_errors_fatal: "{{ false }}"` tras un fallo | el otro host se detiene | como 2.19 | el otro host sigue | como 2.21 | escenario |
| Archivo que falta en `include_tasks` | claves `include` y `reason` | como 2.19 | `msg` y una línea `[ERROR]` | como 2.21 | escenario |
| Origen de `copy` que falta | mensaje de una línea | como 2.19 | `<<< caused by >>>` y las rutas buscadas | como 2.21 | escenario |
| Archivo de rol fuera del rol | código de salida 4 | código de salida 4 | código de salida 1 | código de salida 1 | escenario |
| `debug` de un `msg` no definido | `{"msg": null}` | como 2.19 | `{}` | como 2.21 | escenario |
| `--start-at-task` con una especificación de argumentos de rol | validación omitida | validación ejecutada | como 2.20 | como 2.20 | escenario |
| `ansible --help` | solo `-K` | `-K \| --become-password-file` | como 2.20 | como 2.20 | escenario |
| Valores secretos en la salida | mostrados | mostrados | mostrados | `$REDACTED$`, incluso dentro de una ruta | escenario |
| Valor cifrado con vault en `ansible-inventory --graph` | mostrado | mostrado | mostrado | `$REDACTED$` | inventario |

Con 2.22 el enmascarado también ocultó partes de la salida que no son secretos: la contraseña de become `wrong` convirtió el directorio `become-su-local-wrong-password` en `become-su-local-$REDACTED$-password`.
