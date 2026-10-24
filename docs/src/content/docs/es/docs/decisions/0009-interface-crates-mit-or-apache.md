---
sourceHash: dad3d3f2ec02
title: "Mantener el motor bajo GPL y licenciar los crates de interfaz bajo MIT OR Apache-2.0"
description: "Registro de decisión 0009."
sidebar:
  label: "0009 · Licencias de interfaz"
  order: 9
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-10-24 |

## Contexto

Otros programas quieren dirigir Volant y leer lo que hizo una ejecución: sistemas de CI, editores, paneles y agentes de IA. Hoy solo pueden interpretar la salida para personas, que sigue el formato de texto de Ansible y está escrita para humanos. Está previsto un flujo de eventos documentado, con un crate pequeño, `volant-events`, que contenga sus tipos y su JSON Schema para que un programa en Rust no tenga que definirlos de nuevo.

La [decisión 0001](/es/docs/decisions/0001-gpl-3-0-or-later-license/) puso todo el repositorio bajo GPL-3.0-or-later. Bajo esa licencia, cualquier programa que enlace `volant-events` tendría que ser GPL también, lo que impediría a la mayoría de las herramientas usar una interfaz pensada para todas ellas.

## Decisión

El motor sigue bajo GPL-3.0-or-later. Eso cubre `volant`, `volant-agent`, `volant-protocol` y todo lo demás del repositorio salvo los crates que se indican a continuación.

Un crate cuyo único propósito es describir una interfaz pública para otros programas se licencia bajo MIT OR Apache-2.0. El primero será `volant-events`.

Los demás programas hablan con Volant mediante su línea de comandos y su flujo de eventos documentado. No enlazan los crates del motor.

## Consecuencias

- El motor sigue siendo software libre. Este registro es la promesa del proyecto de que no pasará a una licencia propietaria ni de código disponible.
- Un programa bajo cualquier licencia puede leer los eventos de Volant, mediante `volant-events` o solo a partir del JSON Schema.
- Un crate de interfaz fija `license = "MIT OR Apache-2.0"` en su propio `Cargo.toml` en lugar de heredar la licencia del workspace, y conserva `LICENSE-MIT` y `LICENSE-APACHE` en su directorio. El `LICENSE` de la raíz sigue cubriendo el resto del repositorio.
- Las contribuciones a un crate de interfaz se aceptan bajo MIT OR Apache-2.0, firmadas bajo el Developer Certificate of Origin como cualquier otra contribución.
- Un crate de interfaz se escribe para su propósito y no toma código de ningún crate GPL.
- `volant-protocol` sigue siendo GPL. Pasarlo a otra licencia exigiría el acuerdo de todas las personas que han contribuido a él.
