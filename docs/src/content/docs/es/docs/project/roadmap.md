---
sourceHash: 0f7f2af12690
title: Hoja de ruta
description: Hacia qué trabaja Volant, versión por versión, y por dónde empezar a colaborar.
---

Volant está en fase pre-alfa. El [tablero de la hoja de ruta de Volant](https://github.com/orgs/lorica-labs/projects/1) contiene el plan como issues de GitHub, agrupadas por versión. Cada issue dice qué construir, cómo se comporta ansible-core 2.19.12, cómo comprobar el resultado y por dónde empezar en el código, de modo que cualquiera puede tomar una. El [changelog](https://github.com/lorica-labs/volant/blob/main/CHANGELOG.md) registra lo que se publicó de verdad.

## Versiones

| Hito | Qué aporta |
|---|---|
| [v0.1.0-alpha.11](https://github.com/lorica-labs/volant/milestone/2) | El conjunto de comandos: `volant adhoc`, `volant vault` y `volant inventory` en Rust; `doc`, `config`, `galaxy`, `pull` y `console` mediante el runtime administrado; nombres de comando `ansible-*` opcionales, para que los scripts existentes funcionen sin cambios y sin tener ansible instalado |
| [v0.1.0-alpha.12](https://github.com/lorica-labs/volant/milestone/10) | Un cambio seguro: grabar ejecuciones reales de `ansible-playbook`, reproducirlas con Volant sin tocar los hosts y fijar la versión del motor |
| [v0.1.0-alpha.13](https://github.com/lorica-labs/volant/milestone/3) | Conexiones de contenedores: docker, podman, kubectl, lxd e incus, chroot, buildah, aws_ssm |
| [v0.1.0-alpha.14](https://github.com/lorica-labs/volant/milestone/9) | Trabajo interno: funciones del ejecutor más cortas, tablas declarativas, uso de memoria |
| [v0.1.0-alpha.15](https://github.com/lorica-labs/volant/milestone/8) | Dispositivos de red (network_cli, netconf, httpapi), paramiko_ssh y los demás callbacks de salida estándar |
| [v0.1.0-alpha.16](https://github.com/lorica-labs/volant/milestone/4) | `volant galaxy` en Rust: colecciones y roles |
| [v0.1.0-alpha.17](https://github.com/lorica-labs/volant/milestone/5) | `volant pull` y `volant console` en Rust |
| [v0.1.0-alpha.18](https://github.com/lorica-labs/volant/milestone/6) | Un controlador Windows: `volant.exe` |
| [v0.1.0-alpha.19](https://github.com/lorica-labs/volant/milestone/7) | Hosts Windows: winrm, psrp, runas y los módulos de PowerShell |

[Parity gaps](https://github.com/lorica-labs/volant/milestone/12) enumera las diferencias con ansible-core encontradas hasta ahora. No están ligadas a ninguna versión y cualquiera de ellas se puede corregir en cualquier momento. [Parity gaps, third pass](https://github.com/lorica-labs/volant/milestone/14) reúne las medidas en octubre de 2026: un playbook que con Volant da otro resultado, otro código de salida u otra salida.

## Tomar una issue

Comenta en la issue antes de empezar, para que dos personas no hagan el mismo trabajo. `size/S` es más o menos un día, `size/M` unos días, `size/L` una semana o más, y `good first issue` marca un comienzo suave. [CONTRIBUTING.md](https://github.com/lorica-labs/volant/blob/main/CONTRIBUTING.md) explica la configuración y las comprobaciones.

¿Falta algo que te bloquea? [Abre una solicitud de funcionalidad](https://github.com/lorica-labs/volant/issues/new?template=feature.yml) o inicia un hilo en [Discussions](https://github.com/lorica-labs/volant/discussions).
