---
sourceHash: 8cfc8ab189fd
title: "Usar GPL-3.0-or-later para todo el repositorio"
description: "Registro de decisión 0001."
sidebar:
  label: "0001 · Licencia"
  order: 1
---

| Estado | Fecha |
|---|---|
| aceptada; el alcance se reduce con el [ADR 0009](/es/docs/decisions/0009-interface-crates-mit-or-apache/), que licencia bajo MIT OR Apache-2.0 los crates que describen una interfaz pública | 2026-09-05 |

## Contexto

Volant está pensado para ejecutar módulos de Ansible sin modificar. Para ello, el controlador llamará al constructor de payloads del propio ansible-core mediante un intérprete de Python embebido, y el agente enviará los `module_utils` de ansible-core a los hosts administrados. ansible-core se distribuye bajo la licencia GPL-3.0. Importar una biblioteca GPL en tiempo de ejecución y redistribuir partes de ella hace jurídicamente frágil una licencia permisiva para Volant.

Esta versión todavía no ha llegado ahí. No embebe ningún intérprete de Python, no lleva código de ansible-core y solo ejecuta los módulos nativos. La licencia se elige para la arquitectura hacia la que va el proyecto y no para la que tiene, porque un repositorio cuyos colaboradores han firmado todos bajo una misma licencia es difícil de pasar a otra.

## Decisión

Todo el repositorio se licencia bajo GPL-3.0-or-later, la licencia de Ansible y de la mayoría de las colecciones.

## Consecuencias

- Los colaboradores firman sus commits bajo el Developer Certificate of Origin.
- La adopción no se ve afectada: el propio Ansible es una herramienta de línea de comandos GPL.
- Cuando el constructor de payloads se reescriba en Rust y el agente ya no contenga código de ansible-core, el crate del agente podrá relicenciarse bajo Apache-2.0. Eso será una decisión nueva.

## Actualización, 2026-09-22

El contexto anterior describe el proyecto cuando se tomó esta decisión. Desde el [ADR 0006](/es/docs/decisions/0006-warm-python-path/), Volant envía los `module_utils` de ansible-core a los hosts administrados, y el controlador llama a ansible-core mediante un proceso helper y no mediante un intérprete embebido. El razonamiento de la licencia no cambia.

## Actualización, 2026-10-24

La [decisión 0009](/es/docs/decisions/0009-interface-crates-mit-or-apache/) mantiene el motor bajo GPL-3.0-or-later y licencia bajo MIT OR Apache-2.0 los crates que solo describen una interfaz pública. También mantiene `volant-agent` y `volant-protocol` bajo GPL-3.0-or-later, de modo que el posible cambio de licencia del crate del agente que nombran las consecuencias anteriores no está previsto.
