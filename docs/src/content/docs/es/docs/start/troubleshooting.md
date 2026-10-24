---
sourceHash: 85339820dcff
title: Cuando una ejecución falla
description: Encuentra la tarea que falló y por qué, comprueba el mismo playbook con ansible-playbook e informa de lo que encontraste.
---

Empieza por el código de salida. Te dice si la ejecución se detuvo antes de tocar un host o si falló en uno.

| Código | Qué pasó | Dónde mirar |
|---|---|---|
| 4, sin recap | El playbook se detuvo antes de la primera conexión. No se interpreta, o usa algo que esta versión todavía no admite. | El mensaje que aparece encima del código. Consulta [Detenida antes de la ejecución](#detenida-antes-de-la-ejecución). |
| 2, 4 o 6, con recap | Un host falló (2), un host era inalcanzable (4), o ambas cosas (6). | La línea `fatal:` de la tarea y después [Fallo durante la ejecución](#fallo-durante-la-ejecución). |
| 1 | No se encontró un rol, un playbook o un patrón de hosts. | El archivo o el nombre que cita el mensaje. |

[Códigos de salida](/es/docs/reference/exit-codes/) enumera todos los códigos.

## Detenida antes de la ejecución

Casi todo lo que esta versión no puede ejecutar se nombra antes de que Volant se conecte a nada. El mensaje dice qué rechazó y dónde:

| Mensaje | Qué hacer |
|---|---|
| `task 'NAME': keyword 'KEYWORD' is not supported yet on 'MODULE'` | Consulta la palabra clave en [Palabras clave](/es/docs/reference/keywords/). Quítala, o mantén este play bajo `ansible-playbook` por ahora. |
| `task 'NAME': argument 'ARGUMENT' is not supported yet on 'MODULE'` | Consulta el módulo en [Módulos](/es/docs/reference/modules/). Casi todos los argumentos rechazados tienen una alternativa allí. |
| `couldn't resolve module/action 'NAME'` | Revisa la ortografía. Si el módulo viene de una colección, instálala con `volant runtime install --collections NAMESPACE.COLLECTION`. |
| `strategy 'NAME' is not supported yet` | Usa `linear`, `free`, `host_pinned` o `debug`. |
| `become_method 'NAME' is not supported yet` | Usa `sudo` o `su`. Este termina con el código 2. |
| `the role 'NAME' was not found in ...` | El mensaje enumera los directorios donde buscó. Añade el directorio del rol a `roles_path`. Este termina con el código 1. |

[¿Se ejecutará mi playbook?](/es/docs/start/compatibility/#qué-rechaza-hoy) enumera estos rechazos y muestra cómo comprobar un proyecto entero sin ejecutarlo. [La comprobación previa](/es/docs/internals/preflight/) explica cómo funciona.

## Fallo durante la ejecución

1. Vuelve a ejecutar solo en el host que falla, con más salida. `-vvv` imprime los argumentos que recibió cada módulo:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 -vvv
   ```

2. Salta las tareas que ya pasan. `--start-at-task` empieza en la tarea que nombras, y `--step` pregunta antes de cada tarea:

   ```sh
   volant playbook -i inventory.ini site.yml --limit web1 --start-at-task "Install nginx"
   ```

3. Detente en el fallo y echa un vistazo. Con `debugger: on_failed` en el play o en la tarea, Volant abre el depurador de tareas de Ansible cuando una tarea falla. Puedes imprimir variables, cambiar los argumentos y volver a ejecutar la tarea.

## Comparar con ansible-playbook

Volant lee los mismos archivos, así que el mismo comando funciona con Ansible:

```sh
ansible-playbook -i inventory.ini site.yml --limit web1
```

Si Ansible falla de la misma manera, el problema está en el playbook o en el host. Si solo falla Volant, o si los dos dan resultados distintos, es un error de Volant. Puedes mantener el play bajo `ansible-playbook` mientras se corrige.

## Volver a ansible-playbook

Los mismos archivos funcionan con `ansible-playbook` sin cambios. Volant no añade nada a tus playbooks, roles, inventarios ni a `ansible.cfg`, así que no hay nada que convertir de vuelta.

En los hosts, lo único que queda es el agente en caché: un directorio llamado `volant-agent-<version>-<uid>` en `~/.ansible/tmp`, y a su lado un directorio `volant-blobs-<version>-<uid>` si una ejecución usó módulos de Python. Para eliminarlos, bórralos en cada host, con la cuenta que usó la ejecución:

```sh
ssh web1 'rm -rf ~/.ansible/tmp/volant-*'
```

Un host donde se ejecutaron tareas con `become` guarda una segunda copia en el directorio personal de la cuenta a la que escaló. Ejecuta el mismo comando con esa cuenta. Para quitar también Volant del controlador, consulta [Desinstalar](/es/docs/start/installation/#desinstalar). [Lo que Volant deja en tus máquinas](/es/docs/#lo-que-volant-deja-en-tus-máquinas) lo enumera todo.

## Informar del problema

- Una diferencia con `ansible-playbook` va en un informe de [carencia de compatibilidad con Ansible](https://github.com/lorica-labs/volant/issues/new?template=compat.yml).
- Cualquier otra cosa que no funcione como se documenta va en un [informe de error](https://github.com/lorica-labs/volant/issues/new?template=bug.yml).
- Un problema de seguridad se comunica mediante la [política de seguridad](https://github.com/lorica-labs/volant/blob/main/SECURITY.md), nunca en una incidencia pública.

Incluye la salida de `volant --version`, el playbook e inventario más pequeños que sigan fallando y la salida de la ejecución fallida con `-vvv`. Elimina contraseñas, claves, contenido de vault y nombres de host antes de publicarlos. Una incidencia pública sigue siendo pública.
