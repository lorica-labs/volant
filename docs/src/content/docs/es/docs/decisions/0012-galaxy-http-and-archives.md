---
sourceHash: 9fba53e13fc2
title: "Descargar y desempaquetar el contenido de galaxy dentro de Volant"
description: "Registro de decisión 0012."
sidebar:
  label: "0012 · HTTP y archivos de galaxy"
  order: 12
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-10-27 |

## Contexto

`volant galaxy` instala colecciones y roles sin el ansible-galaxy del runtime gestionado. Para ello habla con un servidor Galaxy por HTTPS, lee y escribe tarballs gzip, clona repositorios git y comprueba firmas GPG. ansible-galaxy 2.19.12 hace las dos primeras cosas en Python, con `urllib` y `tarfile`, y ejecuta los programas `git` y `gpg` para las otras dos. Medido en 2.19.12:

- HTTP sigue las redirecciones (descarga de artefactos), respeta `https_proxy`, `http_proxy` y `no_proxy`, usa el almacén de CA del sistema y envía una cabecera de token cuando hay un token configurado.
- `-vvv` imprime `Downloading <url> to <tmp>` y `Validating downloaded file hash <sha256> with expected hash <sha256>`.
- Un tarball de colección se extrae archivo por archivo según la lista de `FILES.json`, y cada archivo se compara con su `chksum_sha256`. Un archivo que la lista no nombra no se extrae.
- Una fuente git ejecuta `git` y una firma ejecuta `gpg`.

Antes de este registro, Volant no tenía cliente HTTP ni código de archivos. El runtime gestionado ejecuta comandos: `runtime/fetch.rs` lanza `curl` (o `wget`) y luego comprueba un SHA-256 fijado, y `runtime/pack.rs` lanza `tar`. Eso basta para un archivo fijado. Galaxy necesita más: un token en las llamadas a la API que no debe seguir una redirección a otro host, JSON paginado y una extracción que solo escriba lo que lista `FILES.json`.

## Opciones

| Pregunta | Opción | A favor | En contra |
|---|---|---|---|
| HTTP | mantener `curl`, como el runtime | ningún crate nuevo; curl ya lee las variables de proxy y el almacén de CA del sistema | un proceso por petición; el JSON y la paginación pasan por una tubería; el token va en una línea de comandos o en un archivo temporal; `curl.exe` falta en Windows antiguos |
| HTTP | `ureq` con `rustls`, dentro del proceso | las reglas de proxy, redirección y token están en un solo sitio y se comportan igual en todos los sistemas; `http://` simple hacia un servidor de prueba en `127.0.0.1` funciona tal cual | un binario más grande y una compilación más larga (cifras abajo); `ring` compila C, así que una compilación musl necesita un compilador C de musl |
| HTTP | ambos, detrás de una función | el runtime conserva su camino probado | dos caminos que probar para cada regla, y el camino de curl sigue sin poder quitar el token en una redirección a otro host |
| tar y gzip | `tar` del sistema | nada nuevo | no se puede comprobar cada archivo antes de escribirlo; `tar` de GNU y de BSD aceptan opciones distintas; `tar.exe` falta en Windows antiguos |
| tar y gzip | crate `tar` con `flate2` sobre `miniz_oxide` | las entradas llegan una a una, así que Volant decide para cada una si la escribe; `miniz_oxide` ya es una dependencia | unos pocos crates pequeños |
| git y gpg | programas externos | ansible-galaxy hace lo mismo, así que los errores son los mismos | tienen que estar instalados |

## Decisión

### HTTP dentro del proceso, con `ureq` y `rustls`

Las peticiones de galaxy pasan por `ureq` 3, un cliente HTTP/1.1 bloqueante. El TLS es `rustls` con el proveedor `ring`, y las raíces vienen del almacén del sistema a través de `rustls-native-certs`. Las reglas:

- El proxy viene del entorno mediante `ureq::Proxy::try_from_env`, que lee `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` y `NO_PROXY`, en mayúsculas o minúsculas.
- Se siguen las redirecciones. La cabecera del token solo se vuelve a enviar cuando la redirección se queda en el mismo host (`RedirectAuthHeaders::SameHost`).
- `--ignore-certs` (`-c`) desactiva la comprobación de certificados durante la ejecución, como en ansible-galaxy.
- La URL de un servidor puede ser `http://`, como en ansible-galaxy. El servidor Galaxy falso que los tests arrancan en `127.0.0.1` depende de ello.

La descarga del runtime en `runtime/fetch.rs` mantiene `curl` por ahora. Pasarla al mismo cliente es un cambio aparte, y permitiría al controlador de Windows descargar el runtime sin `curl.exe`.

No se usan la característica `platform-verifier` de `ureq` ni su característica por defecto `rustls-webpki-roots`: ambas arrastran `webpki-root-certs` o `webpki-roots`, con licencia CDLA-Permissive-2.0, que `deny.toml` no acepta. `rustls-native-certs` lee el mismo almacén del sistema sin ellas.

### Archivos con los crates `tar` y `flate2`

Los archivos se leen y escriben con el crate `tar`, sin su característica por defecto `xattr`, sobre `flate2` con su `rust_backend`, que es `miniz_oxide`. Instalar una colección lee el tarball entrada por entrada y solo escribe una entrada cuando `FILES.json` la lista y su SHA-256 coincide. Una entrada con ruta absoluta, con un componente `..` o con un enlace que sale del destino se rechaza, como la rechaza ansible-galaxy. Construir un artefacto de colección escribe el tarball con los mismos dos crates.

### `git` y `gpg` siguen siendo externos

Una fuente git ejecuta `git` y una firma ejecuta `gpg`, como en ansible-galaxy. Cuando falta uno, Volant imprime lo que imprime ansible-galaxy 2.19.12 y sale con 1. Medido sin ninguno de los dos en el `PATH`:

```text
[ERROR]: Could not find git executable to extract the collection from the Git repository `https://github.com/ansible-collections/ansible.posix.git`: Failed to find required executable "git" in paths: /tmp/emptybin:/sbin:/usr/sbin:/usr/local/sbin
```

```text
[ERROR]: Failed during GnuPG verification with command 'gpg --status-fd=4 --verify --batch --no-tty --no-default-keyring --keyring=/tmp/gtest/k.kbx - /tmp/gtest/cols/ansible_collections/ansible/posix/MANIFEST.json': [Errno 2] No such file or directory: 'gpg'
```

Las rutas del mensaje de git son el `PATH` seguido de los directorios `sbin` que ansible-core le añade. Galaxy no usa `curl`, así que un `curl` ausente no le afecta.

### Crates que añadir

Cada pull request que añada uno de ellos cita este registro.

- `ureq` 3.5, `default-features = false`, característica `rustls-no-provider`: el cliente HTTP, con las reglas de proxy y redirección de arriba.
- `rustls` 0.23, `default-features = false`, características `ring`, `std`, `tls12`, `logging`: TLS, con el proveedor instalado una vez al arrancar.
- `rustls-native-certs` 0.8: el almacén de CA del sistema, sin un paquete de raíces con licencia CDLA.
- `tar` 0.4, `default-features = false`: lee y escribe entradas tar una a una.
- `flate2` 1.1, `default-features = false`, característica `rust_backend`: gzip sobre el `miniz_oxide` del que Volant ya depende.

## Consecuencias

- El binario de publicación crece 1,6 MB, un 13,4 %, medido abajo. El tiempo de compilación desde cero no cambia más allá del ruido entre ejecuciones.

- Cada regla de galaxy (proxy, token, redirección, certificados, extracción) es código de Volant con sus propios tests, que se ejecutan contra el servidor Galaxy falso por HTTP simple.
- Los trabajos de publicación Linux musl ya instalan `musl-tools` (dist lo añade), y los agentes se construyen con zig, así que el C de `ring` no añade ningún paso a la publicación. Una compilación musl local necesita `musl-gcc` o `cargo zigbuild`.
- Un controlador de Windows puede instalar colecciones sin `curl.exe` ni `tar.exe`.

## Mediciones, 2026-10-27

`cargo zigbuild --release -p volant --target x86_64-unknown-linux-musl` desde un directorio de destino vacío, en una máquina Linux de 4 núcleos, una ejecución cada una. «Con» añade los cinco crates de arriba y una función que descarga un tarball y lo desempaqueta, para que el enlazador los conserve.

| | Sin | Con | Diferencia |
|---|---|---|---|
| Binario sin símbolos | 12.334.584 bytes | 13.988.792 bytes | +1.654.208 bytes (+13,4 %) |
| Compilación desde cero | 546 s | 498 s | dentro del ruido: la compilación con los crates se ejecutó primero y fue la más rápida |
| `cargo deny check` | pasa | pasa | dos avisos nuevos de versiones duplicadas: `getrandom` (0.2, para `ring`) y `windows-sys` |

La compilación para el destino musl suma 21 crates.
