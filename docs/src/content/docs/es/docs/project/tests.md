---
sourceHash: e6b72df6570c
title: Tests
description: Cómo se comprueba Volant contra ansible-core, los tipos de respuesta grabada y cómo añadir un escenario.
---

Volant debe comportarse como ansible-core 2.19.12. Por eso, la mayor parte de la suite de tests es una comparación: ansible-core ejecutó algo una vez, su respuesta se grabó en el repositorio, y el test ejecuta Volant con la misma entrada y compara.

## Los tipos

Cada tipo de grabación tiene un nombre, y `just golden <kind>` lo regenera.

| Tipo | Qué se grabó | Dónde se ejecuta |
|---|---|---|
| `template` | El resultado de renderizar cada expresión de `crates/volant/tests/golden/cases.yml` | Todos los jobs, con `reference.rs` |
| `inventory` | Hosts, variables y grupos de un inventario, y los hosts que selecciona cada patrón | Todos los jobs, con `reference.rs` |
| `scenario` | Una ejecución de playbook: el código de salida y toda la salida estándar | Todos los jobs, con `reference.rs` |
| `shadow` | Una ejecución de un escenario elegido a través del callback `volant.compat.record`, comprimida en `crates/volant/tests/golden/shadow`; el ejemplo propio del callback también se comprueba con cada ansible-core que admite (`just compat-matrix`) | Todos los jobs, con `shadow_golden.rs`, que reproduce cada grabación y no espera ninguna divergencia |
| `module` | Lo que respondió cada módulo nativo en una máquina real | El job `ssh` y `just test-reference`, con `reference_modules.rs` |
| `galaxy` | Un comando `ansible-galaxy` ejecutado contra un servidor Galaxy falso (`golden/reference/galaxy_server.py`): el código de salida, la salida estándar y la de error, las peticiones que vio el servidor y los archivos que quedaron con su SHA-256, en `crates/volant/tests/golden/galaxy/<case>/expected.json` | Todos los jobs, con `galaxy_golden.rs`, que necesita Python 3 para el servidor |
| `command-names` | La salida de los scripts de `tests/command-names`, que llaman a `ansible`, `ansible-playbook`, `ansible-vault`, `ansible-inventory`, `ansible-doc`, `ansible-config`, `ansible-galaxy`, `ansible-pull` y `ansible-console` | El job `command-names` |

Nada de una grabación se edita a mano. Cambia la entrada, ejecuta `just golden <kind>` con ansible-core 2.19.12 instalado y lee el diff. Ejecutarlo en un árbol sin cambios deja los archivos exactamente como estaban. El tipo `module` necesita además `sudo`, systemd, apt y cron, porque crea una cuenta y una unidad en la máquina.

Un caso `galaxy` es un `case.yml` con `args`, los argumentos después de `ansible-galaxy`, donde `<golden-server>` representa la URL del servidor y `<golden-tmp>` el directorio de trabajo de la ejecución. El servidor construye él mismo sus colecciones y roles, con horas y propietarios fijos, así que un archivo tiene el mismo SHA-256 en todas partes. En una grabación, el directorio de trabajo se lee `<golden-tmp>` y el puerto `<golden-port>`. Cualquier otro archivo del directorio del caso se copia en el directorio de trabajo, donde se ejecuta el comando, así que `-r requirements.yml` puede nombrar uno de ellos. `galaxy_golden.rs` reproduce cada petición grabada contra el servidor y ejecuta `volant galaxy` en cada caso cuyo subcomando es nativo.

## El job command-names

El job `command-names` se ejecuta en un contenedor `debian:stable-slim` sin `ansible` y sin Python. Enlaza los nombres `ansible-*` con el `volant` estático mediante `volant shims install`, y luego ejecuta los scripts de `tests/command-names` exactamente como los escribiría un usuario para ansible. Cada script escribe su salida en un archivo y la compara con `tests/command-names/expected/<script>.out`, una grabación de ansible-core 2.19.12 hecha con `just golden command-names`. El job sube los archivos de salida cuando un script falla.

## Añadir un escenario

Un escenario es un directorio dentro de `crates/volant/tests/scenarios`:

1. Crea `crates/volant/tests/scenarios/<name>/` y pon dentro el playbook, el inventario y los archivos que lee el playbook.
2. Escribe `case.yml` a su lado. Una sola ejecución es `args: [-i, inv.ini, site.yml]`. Varias ejecuciones del mismo directorio son `runs:`, una lista de `{args: [...]}`.
3. Ejecuta `just golden scenario`. Escribe `expected.json` en el directorio.
4. Ejecuta `touch crates/volant/tests/reference.rs` y después los tests. La lista de escenarios se fija cuando ese archivo se compila, así que un directorio nuevo no se ve hasta que se recompila.

Cada ejecución de Volant se hace en una copia nueva del directorio, con `--forks 1` salvo que el caso diga otra cosa. El test compara el código de salida y la salida estándar completa, con los tiempos enmascarados. Además de `args` y `runs`, `case.yml` acepta otras seis claves:

- `modes` enumera `check`, `check-diff` y `diff`. Entonces cada ejecución se hace una vez más por modo, con `--check`, `--check --diff` o `--diff` añadido, en ese orden fijo, y cada ejecución se graba.
- `env` define variables de entorno para la ejecución.
- `program: ansible` graba las ejecuciones con `ansible` en lugar de `ansible-playbook`, y ejecuta `volant adhoc` en lugar de `volant playbook`.
- `forks` sube el número de forks por encima de uno. Entonces los hosts pueden imprimir en otro orden, así que enumera en `unordered`, como expresiones regulares, las líneas cuyo orden puede variar. `unordered` sin `forks` se rechaza.
- `needs` nombra lo que debe tener la máquina: `ansible-core`, `sudo` o `systemd`. Un caso cuyas necesidades no se cumplen se omite, salvo cuando `VOLANT_PYTHON` está definida, lo que convierte la omisión en un fallo.
- `links` asocia una ruta con el destino de un enlace que cada copia recibe en esa ruta. Úsalo para un enlace que git o cargo no pueden guardar, como un enlace a un archivo que no existe: `cargo package` rechaza un enlace versionado así.

## La matriz de paridad

`crates/volant/tests/parity.rs` compara cada nombre que conocen ansible-core 2.19.12 y las colecciones del paquete `ansible` fijado (palabras clave, módulos, filtros, tests, lookups y los demás tipos de plugin) con lo que hace el motor con él. `just parity-reference` regenera el lado de la referencia, `crates/volant/tests/parity/reference.json`. Las páginas [Paridad](/es/docs/reference/parity/), [Palabras clave](/es/docs/reference/keywords/) y [Módulos](/es/docs/reference/modules/) se generan a partir de las tablas del propio motor con `just docs-parity`, `just docs-keywords` y `just docs-modules`, y un test falla cuando alguna está desactualizada.

## Ejecutarlos

`just check-touched` es la comprobación que conviene ejecutar antes de una pull request. Formatea, ejecuta clippy y el ratchet de tamaño de archivos, ejecuta los tests de los módulos que cambia la rama y comprueba la ortografía. `just check-local` hace lo mismo con todos los tests. CI ejecuta todos los tests.

`just test` ejecuta todo lo que no necesita ningún host. `just test-reference` ejecuta las grabaciones de módulos contra la máquina en la que estás, lo que necesita ansible-core, sudo y systemd. `just ssh-test` es lo que ejecuta el job `ssh`: parte de un sshd en localhost y cubre también los tests de extremo a extremo. Es preferible un escenario o una fila de tabla en un test existente a una función de test nueva.
