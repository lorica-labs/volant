---
sourceHash: 833100c102c9
title: "Usar ansible-core 2.19 como referencia de compatibilidad"
description: "Registro de decisión 0002."
sidebar:
  label: "0002 · Referencia de compatibilidad"
  order: 2
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-09-05 |

## Contexto

Volant está pensado para ejecutar playbooks existentes sin cambios. "Sin cambios" necesita una definición: ansible-core cambia de comportamiento entre versiones, y la 2.19 reescribió las plantillas (data tagging) con algunas rupturas intencionadas.

## Decisión

Volant reproduce el comportamiento de ansible-core 2.19. Las diferencias con la 2.19 son errores. Las pruebas golden y el arnés de compatibilidad comparan con esa versión. Está previsto, pero no se ha construido, un trabajo nocturno contra la rama de desarrollo de ansible-core para ver los cambios antes de que lleguen a una versión: hoy nada vigila esa rama.

## Consecuencias

- Los playbooks que solo funcionaban con núcleos anteriores por un comportamiento eliminado en la 2.19 quedan fuera del alcance.
- Mover la referencia a un núcleo más reciente es una decisión nueva, registrada aquí, con un cambio de versión mayor o menor según lo que cambie.
