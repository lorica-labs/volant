---
sourceHash: "63370462e057"
title: El runtime administrado
description: El Python y el ansible-core que Volant instala para sí mismo, dónde viven, qué descargan y cómo administrarlos.
---

Volant necesita en el controlador un Python 3 con `ansible-core` para los módulos de Python, la recopilación de facts y los filtros y lookups que no implementa él mismo. Si la máquina no tiene ninguno, Volant instala el suyo, una sola vez, y lo usa a partir de entonces. No necesitas tener ansible instalado para ejecutar un playbook.

El runtime es CPython 3.13 y ansible-core 2.19.12, la versión que reproduce Volant. Vive en un único directorio que es de Volant, así que no toca ningún Python del sistema ni ningún entorno virtual tuyo.

## Cuándo se instala

Una ejecución instala el runtime cuando necesita el helper de Python y no encuentra ningún intérprete utilizable. Imprime una línea en la salida de error y continúa:

```text
Installing Volant's Python runtime (cpython-3.13.16-ansible-core-2.19.12-98a5d8b2) ...
```

Una ejecución que solo recopila facts no instala nada. También puedes instalar el runtime por adelantado, por ejemplo en una imagen:

```sh
volant runtime install
```

En una máquina limpia con una conexión normal, la instalación tardó unos 8 segundos. Las ejecuciones posteriores lo encuentran con dos comprobaciones de archivos.

## Qué intérprete usa una ejecución

El primero de estos que se aplique:

1. `VOLANT_PYTHON`, cuando está definida. Volant usa exactamente ese intérprete y nunca instala.
2. El runtime administrado, cuando está instalado.
3. El entorno virtual activo y después `python3` de tu `PATH`, cuando tienen ansible-core.
4. Un runtime administrado nuevo, que se instala ahora.

Volant inicia el intérprete con `-P`, así que un archivo del directorio de trabajo, como `ansible.py` o `json.py`, nunca se importa en lugar del módulo real. `-P` necesita Python 3.11 o posterior, que es lo que necesita también ansible-core 2.19. Un intérprete propio conserva `PYTHONPATH` y su user site. [`volant doc`](/es/docs/reference/cli/doc/) también elige su intérprete con esta regla, y ejecuta en él el `ansible-doc` propio de ansible-core.

`volant runtime status` muestra la elección que haría la ejecución siguiente, con cada candidato y de dónde salió. El [ajuste `runtime`](/es/docs/reference/configuration/#volant) cambia este orden.

## Qué descarga

Volant obtiene una compilación independiente de CPython y los wheels de ansible-core con `curl`, o con `wget` cuando falta `curl`. También ejecuta `tar`. Las URL y el SHA-256 de cada archivo están fijados dentro del binario de Volant, así que cada versión sabe exactamente qué archivos puede usar.

- Cada descarga se comprueba contra su hash fijado antes de ejecutar nada de lo que contiene. Si no coincide, Volant borra la descarga y se detiene mostrando ambos hashes.
- `pip` instala solo los wheels fijados, con `--require-hashes` y sin índice.
- La instalación se hace en un directorio temporal junto al definitivo y se mueve a su sitio tras comprobar que `import ansible` funciona. Una instalación fallida elimina su directorio temporal. Una interrumpida puede dejar un directorio `.<id>.partial-*`, que la instalación siguiente borra.
- Un archivo de bloqueo impide que dos procesos de Volant instalen a la vez. El segundo espera y después encuentra el runtime instalado.
- Cada proceso hijo se ejecuta con `PYTHONNOUSERSITE=1` y sin `PYTHONPATH`, de modo que los paquetes de tu directorio personal o de tu entorno no pueden llegar a él.

Las plataformas son Linux y macOS en x86_64 y arm64. En Linux, Volant lee la libc de la máquina en la que se ejecuta, glibc o musl, y elige la compilación que corresponde, con independencia de cómo se compiló. Volant no se ha probado en macOS: no había ningún Mac disponible. Cualquier otro sistema se rechaza con un mensaje que lo nombra y apunta a `VOLANT_PYTHON`.

## Dónde vive

Bajo `$XDG_DATA_HOME/volant/runtime`, o `~/.local/share/volant/runtime`. En macOS el valor por defecto es `~/Library/Application Support/volant/runtime`. Cada runtime tiene su propio directorio, nombrado con su identificador (versión de Python, versión de ansible-core y un hash corto de la lista de archivos fijados), de modo que una versión con otras fijaciones se instala junto a la anterior. Un runtime ocupa unos 155 MB.

## Colecciones

Volant no instala una colección durante una ejecución. Para instalar una en el runtime:

```sh
volant runtime install --collections community.general ansible.posix:2.2.2
volant runtime install --collections requirements.yml
```

`--collections` acepta nombres, cada uno con un `:version` opcional, o un archivo de requisitos que termina en `.yml` o `.yaml`. Las colecciones van a `collections/` dentro del runtime. Volant mira allí después de las rutas de `ANSIBLE_COLLECTIONS_PATH` y de `[defaults] collections_path`, así que las colecciones que instalaste tú ganan.

El runtime contiene ansible-core y sus dependencias y nada más. Una colección cuyos plugins necesitan otro paquete de Python, como `jmespath` para `json_query`, no funciona en él. Para esas, usa tu propio entorno con `VOLANT_PYTHON`.

## Comandos

| Comando | Qué hace |
|---|---|
| `volant runtime install` | Instala el runtime si falta. |
| `volant runtime install --collections NAME_OR_FILE...` | Instala además colecciones. |
| `volant runtime install --from FILE` | Instala desde un archivo hecho con `pack`, sin descargar nada. |
| `volant runtime install --from FILE --sha256 HASH` | Instala desde el archivo solo cuando su SHA-256 es `HASH`. |
| `volant runtime pack FILE` | Escribe el runtime instalado en un archivo e imprime su SHA-256. |
| `volant runtime status` | Muestra el modo, el intérprete que usará la ejecución siguiente y el runtime administrado. |
| `volant runtime path` | Imprime el Python del runtime administrado. Sale con código 1 cuando no está instalado. |
| `volant runtime remove [ID]` | Elimina el runtime actual, o el que se nombra. |
| `volant runtime remove --all` | Elimina todos los runtimes. |

### Máquinas sin red

Empaqueta un runtime en una máquina que tenga red, copia el archivo e instálalo en una máquina de la misma plataforma:

```sh
volant runtime pack runtime.tar.gz
# packed runtime.tar.gz
# sha256 3f1c...e9
volant runtime install --from runtime.tar.gz --sha256 3f1c...e9
```

`install --from` rechaza un archivo hecho para otra plataforma u otro runtime, y nombra ambos. `pack` imprime el SHA-256 del archivo en una línea `sha256`. Con `--sha256 HASH`, `install --from` rechaza un archivo cuyo hash difiere, antes de descomprimirlo, y muestra ambos hashes. Sin él, la instalación imprime el hash del archivo que usó, para que puedas compararlo tú. Instalar un paquete ejecuta el Python que lleva dentro, así que pasa el hash que obtuviste de la máquina que lo hizo.

## Desactivarlo

Pon `runtime = system` en la sección `[volant]` de `ansible.cfg`, o `VOLANT_RUNTIME=system`. Entonces Volant usa solo lo que había antes, y un intérprete ausente se rechaza con el mensaje de siempre más una pista sobre el runtime. Con `managed`, Volant usa solo `VOLANT_PYTHON` o el runtime administrado, instala el runtime cuando falta e ignora `python3` en el `PATH`. Consulta [Configuración](/es/docs/reference/configuration/#volant).
