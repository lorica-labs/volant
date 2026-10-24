---
sourceHash: aaa01d3d7aed
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
- `view` imprime el texto en claro de cada archivo. En un terminal pasa por `ANSIBLE_PAGER`, `PAGER` o `less`; `ansible.cfg` no influye en el paginador.
- Sin ninguna fuente de contraseña, el comando la pide. `encrypt` la pide dos veces, como `New Vault password: ` y `Confirm New Vault password: `.
- `-v` imprime el archivo de configuración en uso y `-vv` la versión. Las líneas adicionales de `-vvvv` no se imprimen.

## Carencias conocidas

- Los argumentos `salt` y `wrap_object` del filtro `vault` se rechazan como no admitidos todavía.
- Un valor `!vault` escrito en las propias `vars:` de un play no imprime ninguna línea `Origin:` cuando ningún secreto lo abre.
- Un `set_fact` de un `{{ x }}` desnudo que contiene un valor cifrado pierde la etiqueta de vault, así que `is vault_encrypted` responde falso sobre el fact donde ansible-core responde verdadero.

La [página de la línea de comandos](/es/docs/reference/cli/#vault) tiene las opciones.
