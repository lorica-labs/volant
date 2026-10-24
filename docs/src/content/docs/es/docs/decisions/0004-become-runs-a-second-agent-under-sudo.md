---
sourceHash: 65b041000adc
title: "`become` ejecuta un segundo agente bajo `sudo`"
description: "Registro de decisión 0004."
sidebar:
  label: "0004 · become bajo sudo"
  order: 4
---

| Estado | Fecha |
|---|---|
| aceptada; el límite de conexiones vivas lo sustituye el [ADR 0005](/es/docs/decisions/0005-fork-permits-span-host-local-batches/) y la lista de métodos se amplía a `su` (consulta la actualización de 2026-10-24) | 2026-09-09 |

## Contexto

Ansible escala privilegios por tarea, envolviendo cada invocación de módulo en `sudo`. Volant ejecuta las tareas mediante un agente de larga vida por host, así que envolver cada tarea supondría o bien un agente que llama a `sudo` para cada módulo, o bien un agente que es privilegiado durante todo el play.

## Decisión

El controlador abre una conexión de agente por cada par (host, usuario de destino). Una tarea con `become` se ejecuta mediante el agente iniciado como `sudo -H <form> -u <user> -- volant-agent`, donde `<form>` es `-n` o `-k -S -p ''` según decida la sonda descrita más abajo; las tareas sin `become` conservan el agente sin privilegios. Solo se admite `sudo`; los demás métodos todavía no se admiten y detienen la ejecución. El agente en sí no sabe nada del escalado de privilegios.

## Consecuencias

- Un cambio de usuario de destino entre dos tareas consecutivas termina el lote; el caso habitual, un play entero bajo `become`, mantiene un solo lote.
- La forma de la invocación de `sudo` la fija una sonda, no el hecho de que se haya dado una contraseña. Un `sudo` que no necesita autenticación no lee su entrada estándar, así que en un host con una autenticación en caché o una regla `NOPASSWD`, una contraseña escrita ahí se queda en la tubería y el *agente* la lee como la cabecera de su primer frame. Por eso el controlador pregunta primero con `sudo -n` y solo recurre a `sudo -k -S -p ''` cuando el propio `sudo` ha rechazado la petición por falta de autenticación. Adivinar la forma solo a partir de la contraseña cuelga el enlace, y solo en la segunda conexión con escalado de privilegios dentro de la ventana del timestamp, que es la razón por la que la sonda no es opcional.
- La contraseña, cuando se da una, se escribe una sola vez en la entrada estándar de `sudo` y nunca aparece en una línea de comandos, un log o un resultado. Los dos tipos que la llevan tienen implementaciones de `Debug` escritas a mano que imprimen `<redacted>`, de modo que un volcado con `-vvv` no puede filtrarla, y el error estándar capturado del propio `sudo` se limpia de ella antes de que pueda llegar a un mensaje.
- Un escalado fallido es una tarea fallida, no un host inalcanzable: la conexión funcionó.
- Las conexiones de agente vivas por host no tienen límite. `forks` limita cuántos hosts se ejecutan a la vez, no cuántos enlaces mantiene un host, y el mapa guarda un enlace por cada usuario de destino distinto hasta el recap, cada uno de ellos un proceso `ssh` aparte. Un enlace con escalado de privilegios cuesta además una conexión `ssh` extra para la sonda, dos cuando se quiere una contraseña. Esta decisión no añade multiplexación de conexiones: cada enlace es su propio proceso `ssh`, y un inventario amplio paga por cada uno por separado.
- `su`, `doas`, `pbrun` y los demás pueden añadirse como otras formas de iniciar el agente, sin cambiar el protocolo.

## Actualización, 2026-10-24

`su` se admite desde el 2026-10-16 ([#333](https://github.com/lorica-labs/volant/pull/333)), y sobre una conexión `local` desde el 2026-10-23 ([#755](https://github.com/lorica-labs/volant/pull/755)). Inicia el agente igual que `sudo`, que es la ampliación que esperaba la última consecuencia anterior, y no necesita ningún cambio de protocolo. `doas`, `pbrun` y los demás métodos siguen rechazándose. [Escalado de privilegios](/es/docs/hosts/connections/#become) describe ambos métodos.

El límite de conexiones vivas cambió dos veces después de este registro. El [ADR 0005](/es/docs/decisions/0005-fork-permits-span-host-local-batches/) las ató al permiso de fork. Desde el 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)), un host mantiene su propio enlace más, como máximo, un enlace con escalado de privilegios, sea cual sea el número de usuarios de destino del play. Un escalado a un usuario distinto sustituye al enlace con escalado de privilegios que se tiene en ese momento.
