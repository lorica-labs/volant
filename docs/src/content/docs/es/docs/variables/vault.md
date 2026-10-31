---
sourceHash: c65d7e53b4fd
title: Vault
description: Cómo lee Volant los datos de Ansible Vault, de dónde obtiene las contraseñas y por qué nunca salen del proceso del controlador.
---

Volant lee y escribe los formatos de vault 1.1 y 1.2 de ansible-core 2.19.12. Los resultados, los mensajes de error y los códigos de salida coinciden con los de ansible-core.

## De dónde vienen las contraseñas

| Fuente | Notas |
|---|---|
| `-J`, `--ask-vault-pass` | Se pide en el terminal, sin eco. |
| `--vault-password-file`, `--vault-pass-file` | Un archivo, o un ejecutable que imprime la contraseña. Repetible. |
| `--vault-id LABEL@SOURCE` | La fuente es un archivo, un ejecutable o `prompt`. Repetible. |
| `[defaults] vault_password_file`, `vault_identity_list` | Lo mismo, desde `ansible.cfg`. |
| `ANSIBLE_VAULT_PASSWORD_FILE`, `ANSIBLE_VAULT_IDENTITY_LIST` | Lo mismo, desde el entorno. |

Cada contraseña se prueba con cada valor cifrado, sea cual sea su etiqueta.

## Qué se descifra

- Los valores `!vault` en línea de un playbook o de un archivo de variables. Se cargan como texto cifrado y se descifran cuando una plantilla los lee. Un valor descifrado conserva la confianza que le da ansible-core.
- Archivos enteros cifrados: `vars_files`, `include_vars` y los archivos de `group_vars` y `host_vars`. Un archivo cifrado que no se puede leer falla con el mensaje de ansible-core.
- Los filtros `vault` y `unvault`, los tests `vault_encrypted` y `vaulted_file`, y el lookup `unvault`.
- Un `src` cifrado de `copy`, `template` y `script`, y un archivo comprimido cifrado de `unarchive`. La ejecución lo descifra en un directorio temporal privado antes de que el plugin lo lea. `decrypt: false` en `copy` o `unarchive` conserva el texto cifrado.

Un valor `!vault` que ningún secreto abre hace fallar la tarea que lo lee, con los tres bloques que imprime ansible-core. `debug: var=x` termina con 2, con la causa en la línea donde está escrito el valor. `is vault_encrypted` responde para las variables del host actual.

## Los secretos se quedan en el controlador

Una contraseña de vault vive en memoria en el proceso del controlador. Nunca se escribe en una línea de comandos, en el entorno de un proceso hijo, en un log, en un resultado ni en el enlace con el agente. Las contraseñas de conexión y de escalado de privilegios siguen la misma regla, y [Conexiones](/es/docs/hosts/connections/#tipos-de-conexión) explica cómo llega la contraseña SSH a `ssh`.

Los filtros, tests y lookups que se ejecutan en Python nunca ven la contraseña de vault. Por eso los de vault se ejecutan en el proceso del controlador. Consulta [Filtros, tests y lookups que necesitan Python](/es/docs/variables/templating/#filtros-tests-y-lookups-que-necesitan-python).

Un valor que volvió de un host administrado nunca se descifra con los secretos de la ejecución, y nunca se vuelve a renderizar como plantilla. Consulta [Los valores de los hosts nunca se renderizan](/es/docs/variables/templating/#los-valores-de-los-hosts-nunca-se-renderizan).

## Cifrar y ver archivos

`volant vault encrypt`, `decrypt` y `view` trabajan sobre los mismos archivos que `ansible-vault`: un archivo cifrado por uno lo lee el otro, con el id por defecto y con una etiqueta.

- `encrypt` sustituye cada archivo por su texto de vault y `decrypt` por su texto en claro. Ambos dejan el resultado con modo `0600` y no imprimen nada; en un terminal escriben `Encryption successful` o `Decryption successful` en el error estándar. Un archivo sin salto de línea final vuelve sin él.
- La cabecera es `$ANSIBLE_VAULT;1.1;AES256` para el id por defecto y `$ANSIBLE_VAULT;1.2;AES256;LABEL` para otro. `--vault-id LABEL@SOURCE` fija la etiqueta. Con más de un `--vault-id`, `--encrypt-vault-id` (o `vault_encrypt_identity`, `ANSIBLE_VAULT_ENCRYPT_IDENTITY`) elige el que cifra; sin él, el comando sale con el código 5.
- `--output FILE` escribe ahí y deja intacto el origen, `--output -` escribe en la salida estándar, y sin nombre de archivo, o con `-`, lee la entrada estándar.
- `view` imprime el texto en claro de cada archivo. En un terminal pasa por el paginador: `PAGER`, si no `ANSIBLE_PAGER`, si no `[defaults] pager`, si no `less`.
- Sin ninguna fuente de contraseña, el comando la pide. `encrypt` la pide dos veces, como `New Vault password: ` y `Confirm New Vault password: `.
- `-v` imprime el archivo de configuración en uso y `-vv` la versión. Las líneas adicionales de `-vvvv` no se imprimen.

## Crear, editar y recifrar archivos

`volant vault create`, `edit` y `rekey` trabajan sobre los mismos archivos que `ansible-vault`.

- `create` toma un solo nombre de archivo. Se niega con el código de salida 5 salvo que la salida estándar sea un terminal o se dé `--skip-tty-check`, y con el código de salida 1 cuando el archivo existe. Un directorio que falta se crea, con un aviso. El editor puede dejar el archivo vacío: el resultado es un archivo de vault válido con un texto vacío.
- `edit` descifra cada archivo, abre el editor y cifra el resultado con el secreto que abrió el archivo y la etiqueta que tenía. Si el texto no cambió, el archivo no se reescribe. Un archivo modificado conserva su modo.
- El editor es `EDITOR`, si no `ANSIBLE_EDITOR`, si no `[defaults] editor`, si no `vi`, dividido en palabras como lo haría un shell, con el nombre del archivo añadido. No se mira su estado de salida, como en ansible-vault.
- El editor trabaja sobre una copia con modo `0600` en un directorio privado bajo `ANSIBLE_LOCAL_TEMP` (`[defaults] local_tmp`, si no `~/.ansible/tmp`). La copia se sobrescribe y se elimina en cualquier salida, incluidos un editor que falla y SIGTERM o SIGHUP. Ctrl-C va al editor.
- `rekey` descifra cada archivo con los secretos antiguos y lo escribe de nuevo con el nuevo, conservando su modo. El nuevo secreto viene de `--new-vault-password-file`, de `--new-vault-id LABEL@SOURCE` o de las preguntas `New Vault password: ` y `Confirm New Vault password: `. Imprime `Rekey successful` en el error estándar, sea cual sea la salida estándar.

## Cifrar cadenas

`volant vault encrypt_string` convierte un valor en un bloque `!vault |` para un archivo de variables. Cada línea del texto cifrado lleva diez espacios de sangría, y cada bloque termina con un salto de línea:

```yaml
mykey: !vault |
          $ANSIBLE_VAULT;1.1;AES256
          66313561363937653234366564623639...
```

- `-n NAME` da nombre a la cadena que ocupa el mismo lugar y `--stdin-name NAME` da nombre a la entrada estándar. Sin nombre, el bloque empieza por `!vault |`. `--vault-id LABEL@SOURCE` da una cabecera `1.2` con la etiqueta.
- Sin cadena, o con `-`, el texto se lee de la entrada estándar hasta el final. Una entrada vacía sale con el código 5.
- Con más de una cadena, el error estándar lleva una línea de comentario por cadena, y una nota cuando hay menos nombres que cadenas. La salida estándar contiene solo los bloques.
- `-p` pide un nombre de variable y luego la cadena, que no se muestra al escribirla salvo que se dé `--show-input`. No se puede combinar con la lectura de la entrada estándar.
- En un terminal, `Encryption successful` va al error estándar antes de los bloques.
- `--output FILE` escribe los bloques en el archivo, con modo `0600`.

## Carencias conocidas

- Los argumentos `salt` y `wrap_object` del filtro `vault` se rechazan como no admitidos todavía.
- Un valor `!vault` escrito en las propias `vars:` de un play no imprime ninguna línea `Origin:` cuando ningún secreto lo abre.
- Un `set_fact` de un `{{ x }}` desnudo que contiene un valor cifrado pierde la etiqueta de vault, así que `is vault_encrypted` responde falso sobre el fact donde ansible-core responde verdadero.
- Con `-vvvvv`, `vault view`, `decrypt` y `rekey` no imprimen la traza de ansible-core de cada secreto que prueban (`Trying secret FileVaultSecret(...) for vault_id=...`). Los secretos se prueban en el mismo orden.

La [página de la línea de comandos](/es/docs/reference/cli/#vault) tiene las opciones.
