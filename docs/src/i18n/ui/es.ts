import type { Deep, Strings } from './en';

// Translation of en.ts. A key left out shows in English.
export const es: Deep<Strings> = {
  nav: {
    docs: 'Docs',
    performance: 'Rendimiento',
    compatibility: 'Compatibilidad',
    community: 'Comunidad',
    blog: 'Blog',
  },
  header: {
    home: 'Volant, inicio',
    install: 'Instalar',
    menu: 'Menú',
    releaseNotes: 'Notas de la versión',
    github: 'Volant en GitHub',
    language: 'Idioma',
    skip: 'Saltar al contenido',
  },
  footer: {
    about: 'Un motor para playbooks de Ansible, escrito en Rust. Código abierto bajo GPL-3.0-or-later.',
    use: 'Usar',
    reference: 'Referencia',
    project: 'Proyecto',
    source: 'Código fuente',
    installation: 'Instalación',
    quickstart: 'Inicio rápido',
    willItRun: '¿Se ejecutará mi playbook?',
    whenItFails: 'Cuando una ejecución falla',
    leaves: 'Lo que Volant deja en tus máquinas',
    uninstall: 'Desinstalar',
    cli: 'Línea de comandos',
    configuration: 'Configuración',
    matrix: 'Matriz de compatibilidad',
    changelog: 'Registro de cambios',
    roadmap: 'Hoja de ruta',
    contributing: 'Contribuir',
    security: 'Política de seguridad',
    license: 'Licencia',
    telemetry: 'Volant no recopila telemetría.',
    name: '<i>Volant</i> es «volante» en francés: la pieza con la que se dirige un coche.',
  },
  copy: { copy: 'Copiar', copied: 'Copiado', manual: 'Seleccionar y copiar', label: 'Copiar el comando' },
  lookup: {
    label: '¿Está soportado? Busca un módulo, colección, filtro u opción',
    button: 'Buscar',
  },
  home: {
    title: 'Volant: tus playbooks de Ansible, ejecutados por un motor rápido escrito en Rust',
    description:
      'Volant lee los playbooks, roles e inventarios que ya tienes y los ejecuta en hosts Linux por SSH, mucho más rápido que ansible-playbook.',
    heading: 'Tus playbooks de Ansible, ejecutados por un motor rápido escrito&nbsp;en&nbsp;Rust.',
    lede: 'Volant lee los playbooks, roles e inventarios que ya tienes y los ejecuta por SSH. Nada que convertir.',
    platforms: 'Linux y macOS, x86_64 y arm64. No hace falta Ansible. Windows todavía no está soportado.',
    quickstart: 'Inicio rápido',
    source: 'Código en GitHub',
    badge: 'pre-alpha',
    status:
      'Ejecuta playbooks reales, pero todavía no todos. Pruébalo en un host que puedas permitirte tocar y mira <a class="text-link" href="/docs/#what-volant-leaves-on-your-machines">lo que deja en tus máquinas</a>.',
    runTitle: 'Una ejecución, cinco hosts.',
    runText:
      'Una ejecución real de Volant, reproducida de nuevo. Cada radio de la rueda representa un host y toma el color del resultado que devuelve, con los colores que usa ansible-playbook.',
    howTitle: 'Cómo funciona.',
    howText:
      'ansible-playbook paga un intérprete de Python nuevo en el host por cada tarea. Volant paga una vez por ejecución y mantiene una conexión abierta para el resto.',
    everyTask: 'por cada tarea, en cada host',
    slowSteps: ['construir el payload del módulo', 'enviarlo por SSH', 'iniciar Python', 'importar module_utils', 'ejecutar el módulo'],
    oncePerRun: 'una vez por ejecución',
    onceSteps: ['subir un agente pequeño, en caché en el host', 'abrir una conexión SSH por host'],
    then: 'después',
    forEveryTask: 'por cada tarea',
    fastSteps: ['enviar la tarea por la conexión abierta', 'responderla en Rust, o hacer fork de un Python ya caliente'],
    howNote:
      'Los módulos comunes como <code>apt</code>, <code>systemd</code>, <code>file</code>, <code>lineinfile</code> y la recopilación de facts los responde el agente en Rust cuando los argumentos de la tarea lo permiten. Cualquier otro módulo se ejecuta en Python, en un servidor que cargó sus imports una sola vez. <a class="text-link" href="/docs/internals/architecture/">Cómo está construido el motor</a>',
    filesTitle: 'Los mismos archivos, el mismo comando.',
    filesText:
      '<code>volant playbook</code> acepta las opciones de <code>ansible-playbook</code> e imprime la salida que conoces, hasta el recap. Todo lo que tu proyecto ya tiene se lee tal cual.',
    filesLink: 'Ejecuta tu primer playbook',
    files: [
      ['ansible.cfg', 'se lee de los mismos sitios, con la misma precedencia'],
      ['Inventarios', 'INI, YAML, TOML, scripts y plugins de inventario'],
      ['group_vars y host_vars', 'en capas según el orden de Ansible'],
      ['Vault', 'archivos y valores cifrados, --ask-vault-pass y --vault-id'],
      ['Roles y colecciones', 'dependencias, especificaciones de argumentos y colecciones instaladas'],
      ['Línea de comandos', 'las opciones de ansible-playbook, incluidas --check y --diff'],
    ],
    refusalTitle: 'Dice que no antes de tocar un host.',
    refusalText:
      'Volant comprueba todo el playbook antes de conectarse. La mayor parte de lo que esta versión no puede ejecutar se nombra ahí, con el play o la tarea en que está, y la ejecución se detiene con el código 4. Nada ha cambiado en ningún host.',
    refusalCheck:
      '<code>--syntax-check</code> y los listados nunca se detienen por algo no soportado, y <code>--check</code> con <code>--diff</code> muestra lo que cambiaría una ejecución real. <a class="text-link" href="/docs/start/compatibility/">Comprobar un proyecto</a>',
    refusalCaption: 'Salida real de Volant 0.1.0-alpha.10, para un play que pide una estrategia que esta versión no tiene.',
    runtime: [
      ['Qué descarga', 'su propio Python y ansible-core, unos 155 MB, la primera vez que un playbook necesita Python'],
      ['Cómo lo comprueba', 'cada archivo contra un hash fijado en la versión'],
      ['Dónde lo guarda', '<code>~/.local/share/volant/runtime</code>, aparte de cualquier Python del sistema'],
      ['Colecciones', '<code>volant runtime install --collections community.general</code>'],
      ['Sin red', '<code>volant runtime pack</code> en una máquina, <code>install --from</code> en la otra'],
    ],
    runtimeTitle: 'Sin Ansible que instalar.',
    runtimeText:
      'El binario lleva los agentes que sube a tus hosts. Cuando un playbook necesita un módulo de Python, Volant instala un runtime propio y lo mantiene aparte del resto de la máquina.',
    runtimeLink: 'El runtime administrado',
    faster: 'más rápido en {workload}, medido contra ansible-core con pipelining',
    coreShare: 'de las {total} palabras clave, opciones y plugins de ansible-core se ejecutan hoy',
    blogTitle: 'Del blog.',
    blogAll: 'Todas las entradas',
    promisesTitle: 'Lo que Volant promete.',
    promises: [
      ['Tus archivos siguen siendo tuyos', 'Sin paso de conversión ni formato nuevo. Los mismos archivos siguen funcionando con ansible-playbook.'],
      ['Primero los rechazos', 'La mayor parte de lo que esta versión no puede ejecutar se nombra antes de la primera conexión, y no se ejecuta nada.'],
      ['Primero Linux por SSH', 'Hoy, hosts Linux por SSH y conexiones locales. Windows y los dispositivos de red llegarán después.'],
      ['Sin telemetría', 'Volant no hace ninguna llamada de red aparte de las que pide tu playbook y la descarga del runtime.'],
    ],
    openTitle: 'Construido en abierto.',
    openText:
      'Volant es GPL-3.0-or-later, como Ansible. La hoja de ruta, los issues y las decisiones detrás del diseño son públicos.',
    involved: 'Participa',
  },
  performance: {
    description:
      'Dos cargas reales medidas contra ansible-core 2.19.12 con pipelining, la configuración que hay detrás y los comandos que las reproducen.',
    heading: 'Medido con playbooks reales.',
    lede: 'Dos cargas, cada una lanzada por una receta del repositorio, contra ansible-core 2.19.12 con pipelining activado. Cada cifra de esta página sale de esas recetas, medida el {date}.',
    faster: 'más rápido en {workload}',
    workloads: {
      roles: {
        name: 'Cuatro roles de Galaxy',
        inSentence: 'cuatro roles de Galaxy',
        detail: 'geerlingguy.security, nginx, git y pip en dos hosts Ubuntu 24.04 ya convergidos, 56 tareas por host',
      },
      k3s: {
        name: 'k3s-ansible site.yml',
        inSentence: 'k3s-ansible site.yml',
        detail: 'el playbook oficial de k3s-ansible contra un clúster instalado, un servidor y un agente',
      },
    },
    ratios:
      'Las proporciones están muy separadas por una razón. Los roles no cambian nada en un host convergido, así que casi todo el tiempo de la referencia se va en la maquinaria alrededor de cada módulo. El playbook de k3s reinicia servicios y ejecuta scripts de instalación en cada pasada, y ese trabajo tarda lo mismo con cualquiera de los dos motores.',
    splitTitle: 'En qué gasta Volant su tiempo.',
    splitText: 'La pasada mediana, dividida por pasos. Un paso nativo lo responde en Rust el agente en el host.',
    k3sServer: 'k3s-ansible, el servidor',
    forTwoHosts: '{time} para dos hosts',
    steps: {
      connect: 'Abrir las conexiones',
      files: 'file ×8, lineinfile ×8, stat ×5',
      systemd: 'systemd, 3 tareas',
      setup: 'setup, recopilación de facts',
      apt: 'apt, 8 tareas',
      wire: 'Frames y blobs por la red',
      prepare: 'Preparar las tareas en el controlador',
      moduleSet: 'Construir el conjunto de módulos de Python',
      wait: 'Esperar al otro host',
      restart: 'Reiniciar k3s, servidor y agente',
      serviceFacts: 'service_facts',
      aptCache: 'apt con update_cache',
      packageFacts: 'package_facts',
      facts: 'Recopilación de facts',
    },
    kinds: { native: 'nativo', nativeSystemd: 'systemd nativo', cached: 'en caché', python: 'Python' },
    k3sWork:
      'En k3s, la mayor parte de los 23.5 segundos es trabajo que pide el playbook: reiniciar el servicio de k3s, ejecutar su script de instalación y 13 tareas <code>command</code>. Los dos motores pagan lo mismo por ellas, porque son procesos en el host. <a class="text-link" href="/docs/project/performance/#what-is-left">Lo que queda</a> explica el resto.',
    configTitle: 'La configuración.',
    configText: 'Ajustes por defecto en ambos lados, salvo el pipelining, que hace más rápida la referencia.',
    config: [
      ['Hosts administrados', 'dos hosts Ubuntu 24.04, 4 núcleos cada uno'],
      ['Controlador', 'Ubuntu 26.04, 8 núcleos'],
      ['Volant', 'build de release, ajustes por defecto: módulos nativos activados, una conexión SSH por host'],
      ['Referencia', 'ansible-core 2.19.12 con pipelining = True, forks en su valor por defecto de 5'],
      ['Pasadas', 'una pasada de calentamiento por motor y luego tres pasadas cronometradas cada uno; se informa de la mediana'],
      ['Comprobación', 'cada pasada debe acabar en el mismo PLAY RECAP con los dos motores, o la receta falla'],
    ],
    ownTitle: 'Mide tu propia ejecución.',
    ownProfile: 'En tu propio playbook, <code>--profile</code> imprime el desglose de arriba para tu ejecución, paso a paso.',
    ownRecipes:
      'Para reproducir las dos cargas, las recetas compilan Volant en modo release, se ejecutan contra los hosts que indiques e imprimen las medianas y la proporción. Necesitan ansible-core 2.19.12 instalado con <code>uv tool install</code> y dos hosts donde la cuenta tenga <code>sudo</code> sin contraseña. Las recetas modifican esos hosts, así que usa máquinas que puedas borrar. <a class="text-link" href="/docs/project/performance/#reproducing-the-figures">Las recetas</a>',
    ownRun: '# Your own run, split by step',
    ownBench: '# The two workloads of this page',
    caveat:
      'Estas cifras describen dos playbooks en dos hosts. Las tuyas dependen de tu red, de lo que hagan tus tareas, de cuántas cubre un módulo nativo y de cuántos hosts manejes a la vez. <a class="text-link" href="/docs/project/performance/">La medición completa</a>',
  },
  compatibility: {
    description:
      'Cuánto de ansible-core 2.19.12 y del paquete ansible ejecuta hoy Volant, medido nombre por nombre por la suite de pruebas.',
    heading: 'Compatible con ansible-core 2.19, nombre por nombre.',
    lede: 'Volant carga toda la gramática de ansible-core 2.19 y reproduce su comportamiento, así que cualquier diferencia es un bug. La suite de pruebas lista cada palabra clave, opción y plugin que incluye ansible-core y comprueba qué hace esta versión con cada uno.',
    coreRuns: 'de ansible-core se ejecuta: {runs} de {total} nombres, {partial} más en parte',
    collectionsRun:
      'de los módulos y plugins de las colecciones: {runs} de {total}. La mayoría de los {later} restantes llegan con el soporte de Windows y de dispositivos de red.',
    also: '¿Compruebas un proyecto? <a class="text-link" href="/docs/start/compatibility/">¿Se ejecutará mi playbook?</a> muestra los tres comandos, y las <a class="text-link" href="#known-gaps-note">carencias conocidas</a> listan lo que falla sin mensaje.',
    runs: 'se ejecuta',
    partialLegend: 'parcial: falta un argumento o un valor',
    laterLegend: 'después: Windows, otras conexiones, otros callbacks de salida',
    source: 'Los números se leen de la matriz de compatibilidad que genera la suite de pruebas.',
    groups: {
      language: 'El lenguaje de los playbooks',
      modules: 'Módulos y plantillas',
      hosts: 'Hosts, ejecuciones y línea de comandos',
      collections: 'Colecciones del paquete ansible',
    },
    partial: '{n} parciales',
    later: '{n} para después',
    checkTitle: 'Comprueba tu propio proyecto.',
    checkText: 'Tres comandos, y los dos primeros nunca tocan un host.',
    steps: {
      read: {
        title: 'Léelo sin ejecutarlo',
        note: 'Los listados y --syntax-check nunca se detienen por algo no soportado, así que funcionan con cualquier playbook.',
      },
      list: {
        title: 'Compara la lista de tareas',
        note: 'Si coincide con lo que imprime ansible-playbook --list-tasks, Volant compiló tus roles, imports y tags igual.',
      },
      limit: {
        title: 'Ejecútalo en un host',
        note: 'La mayor parte de lo que esta versión no puede ejecutar detiene la ejecución antes de conectar, nombra lo que rechazó y termina con el código 4.',
      },
    },
    gaps: 'Algunas carencias son silenciosas: una ejecución puede ignorar un ajuste sin error. <a class="text-link" href="/docs/start/compatibility/#known-gaps">Lee las carencias conocidas</a> antes de una ejecución que importe.',
  },
  community: {
    description:
      'Cómo informar de una diferencia con Ansible, tomar un issue, hacer una pregunta o contribuir a Volant, y qué traen las próximas versiones.',
    heading: 'Construido en abierto.',
    lede: 'El código, la hoja de ruta, los issues y las decisiones detrás del diseño son públicos. Se agradecen informes de bugs, correcciones, funciones y documentación.',
    ways: {
      failed: {
        title: 'Mi ejecución falló',
        text: 'Lee el código de salida y el mensaje, acota el fallo a una sola tarea y compara con ansible-playbook. La página te lo explica paso a paso.',
        action: 'Cuando una ejecución falla',
      },
      difference: {
        title: 'Informar de una diferencia',
        text: 'Un playbook que se comporta distinto con Volant y con ansible-playbook es un bug. La plantilla pide lo necesario para reproducirlo.',
        action: 'Abrir un informe de compatibilidad',
      },
      issue: {
        title: 'Tomar un issue',
        text: 'Cada issue planificado da el comportamiento esperado, los criterios de aceptación y por dónde empezar en el código. Coméntalo antes de empezar.',
        action: 'Good first issues',
      },
      ask: {
        title: 'Preguntar o proponer',
        text: 'Las preguntas, ideas y discusiones de diseño van a Discussions. Para algo más grande que una corrección, abre antes un issue y así se acuerda el enfoque.',
        action: 'Ir a Discussions',
      },
      docs: {
        title: 'Mejorar la documentación',
        text: 'Cada página tiene un enlace para editarla y otro para informar de un problema. La documentación vive en el mismo repositorio que el motor.',
        action: 'Leer la documentación',
      },
    },
    setupTitle: 'Listo en cuatro comandos.',
    setupText:
      'El repositorio fija su toolchain de Rust, así que rustup elige la versión correcta. Todo cambio pasa por una pull request, con pruebas de lo que toca.',
    setupLink: 'Leer CONTRIBUTING.md',
    setupHooks: 'hooks de git y las herramientas que usan las recetas',
    setupCheck: 'formato, lints y la suite de pruebas',
    nextTitle: 'Lo que viene.',
    nextText: 'Las versiones planificadas hasta ahora, en orden. Cada una es un hito en GitHub con sus issues.',
    nextLink: 'La hoja de ruta',
    license: 'Licencia',
    licenseText:
      'GPL-3.0-or-later, como Ansible. <a class="text-link" href="/docs/decisions/0001-gpl-3-0-or-later-license/">El registro de decisión 0001</a> explica por qué.',
    security: 'Seguridad',
    securityText:
      'Informa de una vulnerabilidad en privado, nunca en un issue público. <a class="text-link" href="{repo}/blob/main/SECURITY.md">La política de seguridad</a> explica cómo.',
    support: 'Soporte',
    supportText:
      'Dónde preguntar y qué incluir para que alguien pueda ayudarte. <a class="text-link" href="{repo}/blob/main/SUPPORT.md">SUPPORT.md</a>',
    decisions: 'Decisiones',
    decisionsText:
      'Las decisiones que definen lo que ven los usuarios están escritas con sus razones. <a class="text-link" href="/docs/decisions/">Los registros de decisión</a>',
  },
  console: {
    hosts: 'Hosts de la ejecución',
    output: 'Salida de la ejecución',
    source: 'El playbook y el inventario de esta ejecución',
    provenance:
      'Grabado con {version} en cinco hosts mediante una conexión local y reproducido aquí a velocidad de lectura. La salida es lo que se imprimió.',
    states: { waiting: 'esperando', ok: 'ok', changed: 'changed', skipped: 'skipped', failed: 'failed' },
  },
  traces: {
    label: '{workload}: ansible-core se ejecuta durante {ansible} segundos, Volant durante {volant} segundos',
    hint: 'Pasa el cursor sobre la traza para leerla.',
    running: 'en curso',
    done: 'terminado tras {seconds} s',
    caption:
      'Mediana de tres pasadas cronometradas el {date}, contra {reference} con pipelining = True. Una línea está alta mientras dura la ejecución de ese motor.',
  },
  docs: {
    report: 'Informar de un problema con esta página',
    staleTitle: 'Es posible que esta traducción esté desactualizada',
    staleBody: 'La página en inglés cambió después de traducir esta página.',
    staleLink: 'Leer la página en inglés',
  },
};
