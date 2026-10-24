---
sourceHash: 71d56473bdd4
title: "Los permisos de fork abarcan los lotes locales al host"
description: "Registro de decisión 0005."
sidebar:
  label: "0005 · Permisos de fork y procesamiento por lotes"
  order: 5
---

| Estado | Fecha |
|---|---|
| aceptada, y condicionada a una opción desde el 2026-09-20 (consulta Estado más abajo); el límite de enlaces con escalado de privilegios lo sustituye la actualización de 2026-10-24 | 2026-09-16 |

## Contexto

Un driver de host toma uno de los permisos de `forks` antes de abrir una conexión y lo devuelve cuando el lote que reunió ha recibido respuesta. El [ADR 0004](/es/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) dejó sin límite las conexiones de agente vivas; atar los enlaces con escalado de privilegios al permiso es lo que las acota, porque entonces `forks` limita tanto las conexiones abiertas como los hosts que trabajan. Veinte hosts escalando a root con `ulimit -n 128` y `-f 5` solían informar `Too many open files` para un host perfectamente alcanzable.

Devolver los enlaces en cada lote no sale gratis, y un lote termina más a menudo de lo que sugiere la palabra. Cualquiera de estos casos lo cierra: una tarea con `register`, `loop`, `changed_when` o `failed_when`; una tarea que lee el estado de otro host, que es una frontera delante de sí misma; y una tarea que escala a un usuario distinto. Registrar un resultado es algo corriente, así que un play de seis tareas con escalado de privilegios, cada una con un `register`, pagaba seis escalados, cada uno con una sonda de `sudo` y un `sudo` que inicia el agente. Medido en la máquina de desarrollo con un `sudo` que registra lo que se le pide ejecutar: doce invocaciones para esas seis tareas.

Nada de un `register` necesita a otro host, así que el permiso se devolvía delante de una espera que no existía.

## Decisión

Un driver conserva su permiso de fork y sus enlaces con escalado de privilegios de un lote al siguiente cuando está a punto de continuar directamente con un paso propio. Devuelve ambos delante de cada espera a los demás hosts, y al final del play, donde conservarlos solo mantendría ocupados descriptores. Un driver que sale de la ejecución por un fallo también los devuelve.

Cuatro pasos cuentan como una espera: uno que la tabla de palabras clave declara punto de sincronización, uno cuyo texto nombra el estado de otro host, un punto de empalme donde crece la lista de pasos, y un paso cuyo sucesor está detrás de uno. También un paso retenido hasta que su propio resultado decide adónde va el host.

Los enlaces con escalado de privilegios se cierran fuera de la tarea en lugar de esperarse, como antes: `shutdown` concede a su agente hasta dos segundos, y un driver que pagara eso entre lotes serializaría lo que liberar el permiso acaba de liberar.

Esta decisión no añade multiplexación de conexiones. Cada enlace sigue siendo su propio proceso `ssh`.

## Consecuencias

- Los enlaces vivos quedan acotados a uno por host, para la conexión que sobrevive al play, más `forks` veces el número de usuarios de destino distintos a los que escala un driver mientras tiene un permiso. El límite anterior contaba un enlace con escalado de privilegios por driver porque cada lote cerraba el suyo; varios lotes consecutivos bajo un mismo permiso pueden ahora nombrar usuarios distintos.
- Medido, veinte hosts con `-f 5` escalando a root: el controlador alcanza un máximo de 84 descriptores abiertos donde antes llegaba a 69, frente a un límite de 128. La ejecución termina con 0 y sin ningún host inalcanzable, que es la misma prueba con la que se aceptó el límite anterior.
- Un play de seis tareas con escalado de privilegios, cada una cerrando su lote con un `register`, inicia un agente con escalado de privilegios en lugar de seis.
- Los hosts se turnan en una espera y no en cada lote. Con `forks` por debajo del número de hosts vivos y un play cuyos pasos son todos locales al host, un host ejecuta ahora sus pasos restantes antes de que empiece el siguiente, donde antes se alternaban. El trabajo es el mismo y el recap también; lo que cambia es el orden en que llegan las líneas, que ya era desordenado entre dos hosts sin nada que los sincronice.
- Un play que nunca escala privilegios no se ve afectado más allá de conservar su permiso un poco más, y un play en el que todas las tareas leen entre hosts devuelve el permiso en cada paso, como antes.

Sustituye la consecuencia "las conexiones de agente vivas por host no tienen límite" del [ADR 0004](/es/docs/decisions/0004-become-runs-a-second-agent-under-sudo/).

## Estado

Desde el 2026-09-20, un driver lleva su permiso más allá de un paso local al host solo cuando el operador lo pide. `[volant] batching = true` en `ansible.cfg`, o `VOLANT_BATCHING=1`, activa esta decisión; sin ello, cada paso es una espera, como dice `linear` que lo es.

La razón es la cuarta consecuencia anterior. Qué pasos cuentan como una espera se lee del texto de una tarea, y una dependencia que pasa por un archivo, una base de datos o un servicio no deja texto que leer. El `linear` de Ansible promete que los hosts se encuentran delante de cada tarea, y un playbook escrito conforme a esa promesa tiene derecho a apoyarse en ella. Decidir lo contrario solo a partir del texto era el valor por defecto equivocado para un motor cuya primera promesa es que los playbooks se ejecutan sin cambios.

Todo lo que describe la decisión se conserva y sigue ejecutándose bajo la opción. El coste medido de los dos ajustes, y lo que conserva el valor por defecto en ambos casos, están en [Procesamiento por lotes entre hosts](/es/docs/hosts/batching/).

## Actualización, 2026-10-24

La primera consecuencia anterior ya no se cumple. Desde el 2026-09-22 ([#178](https://github.com/lorica-labs/volant/pull/178)), un host conserva su enlace con escalado de privilegios a través de un punto de sincronización, y mantiene su propio enlace más, como máximo, un enlace con escalado de privilegios, sea cual sea el número de usuarios de destino distintos. Un lote que escala de forma distinta al que se tiene (otro usuario, otro programa o indicadores de escalado, otro transporte) cierra el enlace antiguo y abre uno nuevo. El permiso en sí sigue devolviéndose delante de cada espera, como se decidió aquí. [Conexiones y become](/es/docs/hosts/connections/#conexiones-persistentes) expone la regla actual.
