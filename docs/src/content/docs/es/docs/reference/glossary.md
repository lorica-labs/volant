---
sourceHash: 725d6c347d6e
title: Glosario
description: Las palabras que esta documentación usa en un sentido preciso.
---

### Plugin de acción

Código de ansible-core que se ejecuta en el controlador y decide qué enviar a un host para un módulo como `copy` o `package`. Volant ejecuta ocho de ellos en Rust y el resto mediante el bridge. Consulta [Plugins de acción](/es/docs/internals/action-plugins/).

### Agente

Un programa estático pequeño que Volant sube a cada host administrado y deja en caché allí. Recibe tareas del controlador, las ejecuta y devuelve los resultados en un flujo continuo. Consulta [Conexiones y become](/es/docs/hosts/connections/).

### Lote

Las tareas que un host ejecuta seguidas antes de volver a esperar a los demás. Por defecto un lote tiene una tarea de ancho. Consulta [Procesamiento por lotes entre hosts](/es/docs/hosts/batching/).

### Bridge

El camino que sigue un plugin de acción cuando Volant no tiene una versión suya en Rust: el Python propio del plugin, de ansible-core, se ejecuta en el helper del controlador, y el controlador responde a lo que pide. Consulta [En el bridge](/es/docs/internals/action-plugins/#en-el-bridge).

### Controlador

La máquina en la que ejecutas `volant`, y el propio programa `volant`. Lee los playbooks, renderiza plantillas y dirige a los agentes.

### Punto de flush

Un paso en el que se ejecutan los handlers notificados. Cada play tiene tres, y `meta: flush_handlers` añade más. Consulta [Handlers](/es/docs/playbooks/blocks/#handlers).

### Enlace

Una conexión abierta del controlador con un agente. Un host tiene su propio enlace, más uno mientras escala privilegios y uno por cada destino de delegación.

### Runtime administrado

El Python y el ansible-core que Volant instala para sí mismo y usa antes que cualquier intérprete del sistema. Consulta [El runtime administrado](/es/docs/reference/runtime/).

### Módulo nativo

Una versión en Rust de un módulo que lleva el agente. Responde él mismo a una tarea cuando sus argumentos se quedan dentro de lo que implementa, y en caso contrario devuelve la tarea al módulo de Python. Consulta [Módulos nativos](/es/docs/internals/python/#módulos-nativos).

### Comprobación previa

La comprobación que nombra, antes de cualquier conexión, todo lo que esta versión todavía no admite, y detiene la ejecución. Consulta [La comprobación previa](/es/docs/internals/preflight/).

### La referencia

ansible-core 2.19.12, la versión cuyo comportamiento reproduce Volant. Una diferencia con ella es un error, salvo que una página la enumere como deliberada. Consulta el [registro de decisión 0002](/es/docs/decisions/0002-ansible-core-2-19-as-reference/).

### Punto de empalme

Un paso cuyos sucesores solo se conocen mientras el play se ejecuta: un flush de handlers, o un `include_tasks` o `include_role` dinámico. Los pasos nuevos se insertan ahí. Consulta [Imports e includes](/es/docs/playbooks/roles/#imports-e-includes).

### Paso

Una entrada de la lista plana y numerada en la que se compila un play. Todos los hosts recorren la misma lista. Consulta [Ejecución de plays](/es/docs/playbooks/how-a-play-runs/).

### Punto de sincronización

Un paso en el que todos los hosts de un lote esperan a los demás. Con el comportamiento `linear` por defecto, cada tarea es uno.

### Union

El conjunto de módulos que una ejecución puede enviar a un host, construido una vez antes de la primera conexión. Consulta [El camino de Python en caliente](/es/docs/internals/python/).

### Union blob

El único archivo que lleva todos los módulos de Python que necesita una ejecución, con el código compartido que importan. Se envía una vez a cada host. Consulta [El camino de Python en caliente](/es/docs/internals/python/).

### Servidor de Python en caliente

Un proceso de Python que el agente mantiene en ejecución en un host. Carga una vez el código compartido de los módulos y bifurca un proceso hijo por cada tarea.
