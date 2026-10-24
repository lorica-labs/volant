---
sourceHash: d52d376402f0
title: "`ansible_version` informa de la versión de referencia de ansible-core"
description: "Registro de decisión 0003."
sidebar:
  label: "0003 · ansible_version"
  order: 3
---

| Estado | Fecha |
|---|---|
| aceptada | 2026-09-06 |

## Contexto

Muchos roles de Galaxy se ramifican según `ansible_version.full` o `ansible_version.major` para elegir un camino de código. Volant no es Ansible, pero reproduce el comportamiento de una versión de ansible-core (consulta el [ADR 0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/)). Dejar `ansible_version` sin definir haría fallar a esos roles por una variable indefinida; inventar un valor propio de Volant los enviaría por caminos sin probar.

## Decisión

`ansible_version` contiene la versión de referencia que reproduce Volant, con la estructura de diccionario que esperan los roles (`full`, `major`, `minor`, `revision`, `string`). Una variable aparte, `volant_version`, contiene la versión propia de Volant.

## Consecuencias

- Los roles escritos para la versión de referencia se comportan como lo hacen bajo ella.
- Cuando la referencia se mueva (proceso del [ADR 0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/)), `ansible_version` se mueve con ella, en el mismo cambio.
- Un playbook que necesite distinguir Volant de Ansible comprueba `volant_version is defined`.
