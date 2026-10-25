---
sourceHash: 978201df0e63
title: Registros de decisión
description: Las decisiones que dan forma a lo que ven los usuarios de Volant, y por qué se tomaron.
sidebar:
  label: Sobre los registros de decisión
  order: 0
---

Un registro de decisión explica una elección que afecta a los usuarios de Volant: cuál era la situación, qué se decidió y qué se sigue de ello. Los registros usan el formato [MADR](https://adr.github.io/madr/) y están numerados en el orden en que se tomaron.

Un registro no se reescribe una vez aceptado. Cuando una decisión posterior lo cambia, la línea de estado lo indica y enlaza al registro nuevo. Las mediciones hechas después de la decisión se añaden como secciones fechadas al final.

| Registro | Decisión |
|---|---|
| [0001](/es/docs/decisions/0001-gpl-3-0-or-later-license/) | Licenciar el repositorio bajo GPL-3.0-or-later. Los crates de interfaz son la excepción, consulta 0009. |
| [0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/) | Reproducir ansible-core 2.19 y tratar cualquier diferencia como un error. |
| [0003](/es/docs/decisions/0003-ansible-version-reports-the-reference-release/) | `ansible_version` informa de la versión de referencia; `volant_version` informa de la de Volant. |
| [0004](/es/docs/decisions/0004-become-runs-a-second-agent-under-sudo/) | `become` inicia un segundo agente bajo `sudo`. |
| [0005](/es/docs/decisions/0005-fork-permits-span-host-local-batches/) | Un host conserva su permiso de fork a lo largo de los pasos locales al host, tras la opción `batching`. |
| [0006](/es/docs/decisions/0006-warm-python-path/) | Enviar un union blob de módulos de Python por ejecución y crearlos con fork desde un servidor en caliente. |
| [0007](/es/docs/decisions/0007-action-plugins/) | Ejecutar los plugins de acción como una secuencia de subtareas dirigidas por el controlador. |
| [0008](/es/docs/decisions/0008-native-modules-and-speed/) | Responder a los módulos comunes de forma nativa en el agente, dejar el resto a Python y compartir una conexión ssh por host. |
| [0009](/es/docs/decisions/0009-interface-crates-mit-or-apache/) | Mantener el motor bajo GPL-3.0-or-later y licenciar bajo MIT OR Apache-2.0 los crates que describen una interfaz pública. |
| [0010](/es/docs/decisions/0010-core-version-profiles/) | Propuesto: cada proyecto elige la versión de ansible-core que Volant reproduce (`2.19` o `latest`), lo que Ansible retiró se conserva con un aviso, y `volant upgrade-plan` lista lo que cambia una actualización. |
