---
sourceHash: 7f0fcac6cc7d
title: Referencia del lenguaje de plantillas
description: Los métodos de Python, las reglas de cadenas, las expresiones regulares y los límites del Jinja2 de Volant, con todas las diferencias con ansible-core.
---

Esta página enumera lo que puede llamar una plantilla y dónde difiere Volant de ansible-core. [Plantillas](/es/docs/variables/templating/) trata las reglas de uso diario y los filtros, tests y lookups que ofrece Volant.

## Fecha y texto de relleno

`now(utc=False, fmt=None)` da la fecha y la hora del controlador, nunca de un host administrado. Devuelve un `datetime` sin zona horaria, en UTC cuando `utc` es verdadero. Cuando se define `fmt`, devuelve en su lugar el texto que da `strftime(fmt)`, así que `{{ now(utc=true, fmt='%Y%m%d') }}` es `20261019`. El `datetime` tiene los atributos `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond` y `tzinfo`, los métodos `strftime`, `isoformat`, `timestamp`, `weekday` e `isoweekday`, y se puede comparar con otro `datetime`. Como resultado o mediante `to_json` se imprime como `2026-10-19T23:13:50.741151`. En un texto, mediante `string` o después de `~`, se imprime con un espacio en lugar de la `T`, como hace Python. `strftime` usa siempre la configuración regional C: los nombres de días y meses están en inglés y `%c` da `Mon Oct 19 23:13:50 2026`. ansible-core usa la configuración regional del controlador.

`lipsum(n=5, html=True, min=20, max=100)` da `n` párrafos de texto de relleno aleatorio, como hace Jinja. Cada uno tiene al menos `min` palabras y menos de `max`. Con `html` verdadero cada párrafo va entre etiquetas `<p>` y los separa un salto de línea; en otro caso los separa una línea en blanco. Una llamada genera como máximo 10 millones de palabras.

## Métodos de Python

Una plantilla puede llamar a los métodos que Python da a sus valores, como en ansible-core. Un método que Volant no ejecuta falla con su propio nombre, así que nada se omite sin un mensaje.

Estos métodos de `str` se ejecutan, con los argumentos y palabras clave que acepta Python y con las palabras de Python para una llamada incorrecta (`must be str or None, not int`, `substring not found`):

| Grupo | Métodos |
|---|---|
| Cortar y unir | `split`, `rsplit`, `splitlines`, `partition`, `rpartition`, `join`, `expandtabs` |
| Recortar y rellenar | `strip`, `lstrip`, `rstrip`, `removeprefix`, `removesuffix`, `center`, `ljust`, `rjust`, `zfill` |
| Buscar | `find`, `rfind`, `index`, `rindex`, `count`, `startswith`, `endswith`, `replace` |
| Mayúsculas y minúsculas | `upper`, `lower`, `title`, `capitalize`, `swapcase`, `casefold` |
| Comprobar | `isalpha`, `isalnum`, `isdigit`, `isdecimal`, `isnumeric`, `isspace`, `islower`, `isupper`, `istitle`, `isascii`, `isidentifier`, `isprintable` |
| Otros | `format`, `format_map`, `translate`, `maketrans`, `encode` |

Las posiciones cuentan caracteres, no bytes, y `start` y `end` aceptan valores negativos. `split(maxsplit=1)` y las demás formas con palabra clave funcionan. `startswith` y `endswith` aceptan un texto o una tupla de textos. Una lista se acepta como tupla, porque el Jinja de Volant no tiene tipo tupla.

En qué difiere de CPython:

- `casefold` es minúsculas más `ß` a `ss`. No es el plegado completo de mayúsculas y minúsculas de Unicode.
- `isdigit`, `isalpha` y los demás tests fuera de ASCII siguen las tablas Unicode de Rust. `'½'.isdigit()` es verdadero aquí y falso en Python.
- Un campo de formato puede leer un elemento (`{0[a]}`, `{a[0]}`) pero no un atributo (`{0.real}`).
- Un ancho, un recuento o una precisión que haría que un método construyera más de 256 MiB de texto (`'a'.ljust(10000000000)`, `'{:.300000000f}'.format(x)`) detiene la tarea con un error que nombra el límite. Python construye el texto hasta que la máquina se queda sin memoria. Un número demasiado grande para un índice da el propio error de Python, `Python int too large to convert to C ssize_t`.
- `encode` produce un valor bytes, y Volant ejecuta solo unos pocos métodos de bytes. Consulta [Bytes](#bytes).
- Una secuencia de escape con barra invertida como `'\n'` dentro de una cadena de plantilla la lee el motor de plantillas. ansible-core 2.19 conserva los dos caracteres. Pasa un valor en una variable cuando el texto deba contener un salto de línea real.

### Listas y diccionarios

También se ejecutan los métodos de `list` y `dict`: `append`, `extend`, `insert`, `remove`, `pop`, `sort`, `reverse`, `index`, `count`, `copy` y `clear` en una lista, y `get`, `keys`, `values`, `items`, `pop`, `popitem`, `setdefault`, `update`, `copy`, `clear` y `fromkeys` en un diccionario. `fromkeys` es también un método del nombre `dict`: `dict.fromkeys(['a', 'b'], 0)`. `keys()`, `values()` e `items()` dan listas. `sort` acepta `reverse`, y `key` solo como `none`. Las formas habituales de escribir un cambio son `{% set _ = items.append(x) %}` y `{{ items.append(x) or '' }}`: el método cambia la lista y responde `none`.

Una plantilla cambia una lista o un diccionario en el sitio, y los demás nombres del mismo valor ven el cambio:

```jinja
{% set seen = [] %}
{% for host in groups['web'] %}{% set _ = seen.append(host) %}{% endfor %}
{% set alias = seen %}{% set _ = alias.append('db') %}
{{ seen }}   {# every web host, then db #}
```

Esto sigue a Python y a ansible-core 2.19, que coinciden en estos casos (cada caso se prueba contra ansible-core):

- Un valor leído de una variable es una copia para la cadena que se está renderizando. Una variable de play `x` que una plantilla cambia con `x.append(1)` se cambia solo para esa plantilla. La tarea siguiente, el campo siguiente de la misma tarea y la plantilla siguiente la leen como estaba. Ningún cambio en una variable sobrevive al renderizado.
- Dentro de una plantilla, `b = a` nombra una sola lista, y un cambio mediante `b` se ve en `a`. `b = a | list` y `b = a.copy()` crean una lista nueva.
- Una lista o un diccionario dentro de otro se alcanza con `x[0]`, `x.key` y `for item in x` como el mismo objeto, así que `d.a.append(1)` cambia `d`. Un valor guardado con `append`, `extend`, `insert`, `update` o `setdefault` es una copia, y también lo son los valores que responden `get`, `items`, `values` y `copy`.
- `d.setdefault('k', []).append(1)` cambia `d`. `v = d.setdefault('k', [])` seguido de `v.append(1)` cambia `d` aquí, y no lo cambia en ansible-core.

En qué difiere de ansible-core:

- Una lista o un diccionario que una plantilla pasa a una macro, a `namespace()` o a otra llamada es el mismo objeto en Volant. ansible-core pasa una copia, así que una macro que cambia su argumento cambia la lista del llamador solo aquí.
- ansible-core 2.19 conserva el error de una llamada cuyo resultado nunca se lee. `{% set _ = [1].remove(5) %}` no es un error allí. Aquí sí lo es.
- Una lista que construyó `a + b` o un segmento `a[1:]` no se puede cambiar: escribe `(a + b) | list`. El error lo dice.
- Una lista que una llamada acaba de construir y que ninguna variable contiene, como `(x | list).append(1)`, se cambia como copia: la llamada responde como lo hace Python, y nada más cambia.
- Una lista que crece dentro de un `{% for %}` sobre ella misma se lee hasta su longitud al empezar el bucle. Una que `remove` o `pop` reducen dentro de un bucle así falla con un valor no definido, donde ansible-core continúa.
- Una lista que responde un filtro es una copia, como en ansible-core, así que `(items | first).append(1)` no cambia `items`. Un segmento (`a[1:]`) comparte con `a` las listas que contiene.
- Una lista o un diccionario que es el elemento o la clave de su propio método se lee como una copia de sí mismo, y un diccionario usado como clave es un error (`unhashable type: 'dict'`), como en Python.
- `sort()` de una lista que contiene `nan` es un error aquí. ansible-core la ordena.
- `{% do %}` se acepta aquí y ansible-core 2.19 lo rechaza, porque no habilita `jinja2.ext.do`. Usa `{% set _ = ... %}` en una plantilla que tenga que funcionar en ambos.
- `items()` y `popitem()` dan listas donde Python da tuplas, así que se imprimen como `['a', 1]` y no como `('a', 1)`.
- `sort(key=...)` con una función no está disponible.

Métodos como `.get()`, `.keys()`, `.values()` y `.items()` funcionan también en otros valores parecidos a diccionarios, como `hostvars` y la variable de bucle.

### Bytes

`'text'.encode(encoding='utf-8', errors='strict')` produce un valor bytes:

- `| length` cuenta los bytes. `b[0]`, `b[-1]`, `| first`, `| list` y `{% for %}` dan enteros.
- `b.decode(encoding, errors)` y `b.hex(sep, bytes_per_sep)` dan texto. `.encode('ascii', 'ignore').decode('ascii')` elimina los caracteres que no son ASCII.
- `b64encode` lee los bytes exactos. `b64decode`, `regex_replace`, `regex_search`, `regex_findall` y `quote` los leen como texto UTF-8.
- Un resultado que es bytes se convierte en su texto, y también `x {{ b }}`. `'x' ~ b`, `b | string` y `'%s' % b` dan `b'abc'`, como en Python.
- `==`, `is sequence`, `is string` y `type_debug` (`bytes`) responden como en ansible-core. `to_json` y `to_nice_json` fallan con `Object of type 'bytes' is not JSON serializable`.

`encode` conoce `utf-8`, `ascii`, `latin-1`, `utf-16` y `utf-32` (también `-le` y `-be`), con los nombres alias que lee Python (`UTF-8`, `utf8`, `iso-8859-1`). Los manejadores de errores son `strict`, `ignore`, `replace`, `backslashreplace` y `xmlcharrefreplace`. `decode` conoce `utf-8`, `ascii` y `latin-1`, con `strict`, `ignore`, `replace` y `backslashreplace`. Un error usa las palabras de Python: `'ascii' codec can't encode character '\xe9' in position 0: ordinal not in range(128)`.

### El operador `%`

En un texto, `%` lo formatea como lo hace Python: `'%s-%d' % (name, 3)`, `'%(host)s:%(port)d' % item`, `'%5.2f%%' % pct`. Se leen los indicadores `-`, `+`, espacio, `0` y `#`, un ancho y una precisión (cada uno puede ser `*` para leerlo de los argumentos), los modificadores de longitud `h`, `l` y `L`, y las conversiones `s`, `r`, `a`, `c`, `d`, `i`, `u`, `o`, `x`, `X`, `e`, `E`, `f`, `F`, `g`, `G` y `%`. Una llamada incorrecta falla con las palabras de Python: `not enough arguments for format string`, `not all arguments converted during string formatting`, `%d format: a real number is required, not str`.

Los enteros de hasta 128 bits que una plantilla escribe o calcula se leen con exactitud (`'%d' % 18446744073709551615`). Un entero de más de 63 bits en un archivo de variables llega a la plantilla como texto, lo cual decide el lector de YAML, no `%`. Un ancho o una precisión que pasen de 256 MiB de texto es un error de tarea que nombra el límite, como en los métodos de `str`, y uno que pase de lo que cabe en un índice es el `width too big` de Python.

En dos números, `%` toma el signo del divisor, como en Python: `7 % -3` es `-2`. Un divisor cero es un error. El filtro `format` es `%` con los argumentos escritos explícitamente, y lee los mismos formatos.

Volant no tiene tipo tupla, así que el lado derecho de `%` se lee como una tupla cuando está escrito explícitamente en la plantilla, como `(a, b)`, `(a,)` o `['a', 'b']`. Una lista que guarda una variable o que devuelve una llamada es un solo argumento, como en Python: `'%s' % items` imprime la lista. `'%s %s' % pair` con una lista de dos elementos en `pair` es un error en ansible-core, y aquí también. Una tupla se imprime como una lista: `'%s' % ((1, 2),)` es `[1, 2]` aquí y `(1, 2)` en ansible-core.

Dos números escritos explícitamente en la plantilla (`7 % -3`) se pliegan cuando la plantilla se compila, y un plegado no sigue la regla de signos de Python ni falla con un divisor cero. Un número que viene de una variable sí.

## Filtros de conjuntos

`difference`, `intersect` y `union` construyen un `set` de Python en ansible-core, cuyo orden es el suyo: los enteros pequeños salen ordenados por casualidad, y las cadenas salen en un orden que cambia con `PYTHONHASHSEED` de una ejecución a otra. Volant ordena el resultado cuando todos los elementos son enteros, lo que coincide con todos los casos de enteros medidos, y en otro caso conserva el orden en que los elementos aparecieron por primera vez. En cadenas, no se garantiza que ese orden coincida con una ejecución concreta de ansible-core.

## Cadenas que contienen bloques

Volant renderiza cualquier cadena que contenga `{{`, `{%` o `{#`, en una variable, un fact, un argumento de tarea, un bucle o una variable adicional. Las renderiza como lo hace ansible-core:

- `trim_blocks` está activado y `lstrip_blocks` desactivado. El salto de línea después de una línea `{% ... %}` se elimina, así que un escalar de bloque con un bucle `{% for %}` da una línea por cada pasada y ninguna para las etiquetas.
- Un resultado formado por un único `{{ }}` y nada más conserva su tipo, incluso cuando lo rodean sentencias o comentarios. `{% set a = [1, 2] %}{{ a }}` es una lista. Un resultado formado por texto, o por varias expresiones, es una cadena.
- Un resultado sin ninguna salida es `none`. `{% if false %}x{% endif %}` y `{# note #}` son `none`, no una cadena vacía.
- Un salto de línea al final de la cadena no forma parte del resultado cuando el resultado no es una cadena: `"{{ [1, 2] }}\n"` es una lista. `{{-` y `-}}` eliminan el espacio en blanco alrededor de la etiqueta, así que `{{- x -}}` con un salto de línea detrás también es un solo valor, y el `-` nunca es un signo menos.
- `{#` inicia un comentario. Un comentario que se abre justo al final de la cadena y nunca se cierra se acepta y no significa nada. Cualquier otra cosa sin terminar es un error de sintaxis. Para escribir `{#` como texto, usa `{{ '{#' }}`: un resultado que contiene `{#` no se vuelve a renderizar, y tampoco una variable o un fact guardado cuyo `{#` no tiene un `#}` después.
- Una cadena como `echo ${#items[@]}` falla con un error de sintaxis, como en ansible-core. Envuélvela en `{% raw %}` o márcala `!unsafe`. ansible-core rechaza una tarea `command` o `shell` que contiene una así mientras carga las tareas, con código de salida 4. Volant la rechaza cuando la tarea se ejecuta, con código de salida 2, así que las tareas anteriores se ejecutan primero.

Un resultado registrado y cualquier otro texto de un host administrado sigue siendo texto: nunca se renderiza. Consulta [Valores fiables y no fiables](#valores-fiables-y-no-fiables). Un valor bajo `!unsafe` son datos y nunca se renderiza como plantilla. Un valor `!vault` se descifra, consulta [Vault](/es/docs/variables/vault/).

## Expresiones regulares

`regex_replace`, `regex_search`, `regex_findall` y los tests `match`, `search` y `regex` leen patrones `re` de Python. Volant reescribe el patrón para el crate `regex` de Rust y recorre las coincidencias como lo hace Python 3.7 y posteriores. El tratamiento de los grupos, las coincidencias vacías, las cadenas de sustitución (`\1`, `\g<name>`, un `$` literal) y los argumentos, incluidos los indicadores por posición, siguen a ansible-core.

Parte de la sintaxis de Python no tiene equivalente en el crate de Rust. Un patrón que la usa falla con un error que nombra la construcción, donde ansible-core lo ejecuta:

- anticipación y retroceso (look-ahead y look-behind): `(?=`, `(?!`, `(?<=`, `(?<!`
- una referencia hacia atrás dentro del patrón: `\1`, `(?P=name)`
- cuantificadores posesivos (`a++`), grupos atómicos (`(?>`) y grupos condicionales (`(?(1)a|b)`)
- `\N{name}`, los indicadores `(?a)` y `(?L)`, y `(?x:` como indicador con ámbito
- una repetición perezosa o una alternativa vacía antes de otra (`a*?`, `|a`) cuando una coincidencia vacía necesita que se pruebe una más larga en el mismo lugar
- `$` en un patrón sin `multiline`, en un texto que tiene más de una línea y termina con un salto de línea

Quedan dos diferencias. Una coincidencia con varios grupos es una lista de listas aquí y una lista de tuplas en ansible-core, lo que solo se nota cuando el valor se imprime como texto. El motor de plantillas de Volant lee `'\\'` en un literal de cadena como una sola barra invertida, donde ansible-core conserva dos.

## Llaves literales

El texto que escribe un bloque `raw`, o una cadena como `'{{'`, es literal: no se vuelve a renderizar como argumento, en una variable, en un valor de `set_fact` ni en un elemento de bucle. Una variable que contiene un bloque `raw` se lee primero hasta su final, así que `a: "{{ b }}"` con `b: "{% raw %}{{ x }}{% endraw %}"` da `{{ x }}`.

Las llaves que construye una expresión también son literales. Con `x: a`, `{{ x ~ '{{ 1 + 1 }}' }}` da `a{{ 1 + 1 }}`, `{{ x | replace('a', '{{ 7 * 6 }}') }}` da `{{ 7 * 6 }}`, y `{{ '{{ ' ~ n ~ '_port }}' }}` da `{{ web_port }}` sea cual sea el valor de `web_port`. Una variable que contiene una plantilla se renderiza cuando se lee, así que una cadena como `a: "{{ b }}"` con `b: "{{ c }}"` da el valor de `c`. Un resultado se vuelve a renderizar solo cuando una lectura le dio una plantilla que todavía no estaba renderizada: `hostvars['other']['name']` para una variable que contiene una, o una variable que se dejó como estaba escrita por la regla anterior. Un texto que une una lectura así con llaves que construye se renderiza entonces en su conjunto.

## Filtros que se ejecutan en Python

[Plantillas](/es/docs/variables/templating/#filtros-tests-y-lookups-que-necesitan-python) dice qué nombres necesitan Python. Estos son los detalles.

Un nombre en una línea `import_playbook` pasa también por Python, renderizado con las variables de `-e`.

Un plugin de Python que lanza una excepción falla con la cadena de ansible-core: la frase del plugin en un bloque y el mensaje de la excepción en el siguiente. Un filtro o un test que lanza una excepción imprime la misma cadena en un argumento de tarea, en un `when` y en un archivo `template`.

Volant renderiza primero todos los argumentos y envía el resultado como datos. Python no lo vuelve a renderizar como plantilla, así que un valor que vino de un host administrado y todavía contiene `{{ ... }}` se queda como texto plano. Los valores marcados `!unsafe` llegan también como datos.

Un lookup que se ejecuta en Python recibe además las variables del play. Volant lee el código fuente del plugin y nombra las variables que lee: las claves literales que busca en `variables` y las que nombran sus ajustes. Un plugin que enumera variables, como `community.general.merge_variables`, las recibe todas. Solo un plugin así paga el envío de todas las variables del host.

Cada uno de estos casos hace fallar la tarea que hizo la llamada, con el mensaje del proceso de Python en el error de la tarea:

- El proceso de Python termina o se mata durante un renderizado.
- Una petición o una respuesta es mayor que el límite de frame.
- Un plugin responde bytes que no son UTF-8.
- El proceso de Python responde a otra petición.

La llamada siguiente de la ejecución que necesite Python falla del mismo modo en lugar de esperar.

## Valores fiables y no fiables

Una plantilla que escribiste es código. Un valor que devolvió un host administrado son datos. Volant mantiene separadas ambas cosas como lo hace ansible-core 2.19, de modo que una cadena que controla un host nunca se ejecuta como plantilla en el controlador.

:::caution
Esto es una frontera de seguridad. Si encuentras una forma de que los datos de un host se evalúen como plantilla en el controlador, infórmalo en privado como describe la [política de seguridad](https://github.com/lorica-labs/volant/blob/main/SECURITY.md).
:::

### Qué valores no son fiables

| No fiable, porque lo produjo un host o la ejecución | Fiable, porque lo lleva el playbook |
|---|---|
| resultados de `register` y resultados de módulos | `vars` de play y de tarea |
| facts recopilados | `--extra-vars` |
| valores de `set_fact` | variables del inventario |
| `lookup('file')`, `lookup('pipe')`, `lookup('env')`, `lookup('template')` | `vars_files` e `include_vars` |

`!unsafe` marca como datos un valor del playbook. Se acepta en un valor y en una clave de diccionario, en un play, en `group_vars`, en un archivo de extra-vars y en un inventario YAML. El filtro que usa el cargador de YAML para la marca está oculto a los playbooks: llamarlo falla con `No filter named 'volant_unsafe'`.

### Qué cambia con un valor no fiable

Un valor no fiable se renderiza una vez. Si el resultado vuelve a parecer una plantilla, se queda como texto y no se renderiza una segunda vez, que es lo que hace Ansible:

```yaml
# Say someone wrote {{ lookup('pipe', 'id') }} into /etc/motd on the host.
- command: cat /etc/motd
  register: motd

- debug:
    msg: "{{ motd.stdout }}"   # prints the braces as text, runs nothing
```

`debug: var:` evalúa una expresión. Cuando el nombre mismo vino de un host, la tarea falla con el mensaje propio de ansible-core:

```text
Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.
```

Escribir tú el nombre, como en `debug: var: result.stdout`, funciona como siempre. Lo que se detiene es `debug: var: "{{ from_a_host }}"`.

Las condiciones siguen la misma regla. Un `when: "{{ ... }}"` que se renderiza como una cadena que escribiste, como `"1 == 1"`, se evalúa una vez más como condición. Si el renderizado leyó algo que envió un host, la cadena nunca se compila, y la tarea falla con el mismo `Encountered untrusted template or expression.` Un `assert` cuyo `that` es una plantilla funciona igual.

El resultado de un `lookup('template')` también son datos. Una plantilla que contiene `{{ '{{ 1 + 1 }}' }}` da el texto `{{ 1 + 1 }}`, no `2`, con ambos motores.

Un `delegate_to` que se renderiza a partir de un valor de un host nombra un host con ese texto, sea cual sea su contenido. El texto es el `inventory_hostname` y el `inventory_hostname_short` del destino, y nada lo renderiza, ni siquiera cuando una variable de tu tarea pone llaves junto a él. Lo mismo vale para un nombre que contiene llaves venga de donde venga, y para un destino que no es un host del inventario, como uno que creó `add_host`. Por ssh la tarea falla con `UNREACHABLE` e imprime el texto, como en ansible-core. Un destino que se conecta de forma local se ejecuta como siempre.

Una ruta que nombra un host administrado es un caso aparte, porque el controlador la lee o la escribe. Por defecto el `src` de `copy`, `template`, `script`, `unarchive` y `assemble` y el `dest` de `fetch` se comportan como en ansible-core. Dos ajustes los restringen: [`host_named_paths` para un archivo de origen](/es/docs/internals/action-plugins/#un-archivo-src-nombrado-por-un-host-administrado) y [`host_named_dest_dirs` para un `dest`](/es/docs/internals/action-plugins/#un-dest-nombrado-por-un-host-administrado).

### Cuánta granularidad tiene

Volant sigue la confianza por variable, no por cadena. Ansible etiqueta cada objeto cadena, así que una cadena fiable conserva su confianza a través de una plantilla y la pierde solo cuando una expresión construye una cadena nueva a partir de ella. En Volant, un valor es no fiable cuando el renderizado que lo produjo leyó algo que aportó un host. La respuesta es la misma dondequiera que un host participó. Volant es más permisivo con un valor que el playbook calculó a partir de su propio texto.

Un host administrado tampoco puede cambiar sus propios ajustes de conexión mediante los facts que devuelve un módulo. Volant elimina los nombres que elimina ansible-core, como `ansible_host`, `ansible_user` o `ansible_ssh_common_args`, además de `ansible_remote_tmp`, e imprime el mismo aviso.

## Límites de YAML

Volant rechaza un texto YAML que no puede cargar sin riesgo. El error nombra el límite. Un playbook, un archivo de variables, un inventario y el texto que lee un filtro `from_yaml` siguen los mismos límites.

- Un documento anida colecciones como máximo 489 niveles. ansible-core carga un archivo hasta 489 niveles y lo detiene en 490 con `maximum recursion depth exceeded`, y Volant da el mismo mensaje. Un alias cuenta como los niveles del nodo que nombra. Una colección en línea (flow) se detiene antes, en 255 niveles, porque el analizador los cuenta en un `u8`. ansible-core carga una colección en línea hasta 489, así que un archivo con muchas colecciones en línea entre 256 y 489 niveles sigue siendo distinto.
- Un texto contiene un documento, como en ansible-core: un segundo `---` detiene la carga. Los límites siguientes cuentan el texto completo.
- Los anclajes y alias de un texto copian como máximo 1,000,000 de nodos y 64 MiB de texto escalar. Se hace una copia en cada anclaje y en cada alias. ansible-core comparte un nodo y no copia nada, así que carga un texto que representa miles de millones de nodos y falla solo cuando una tarea recorre el valor. Por ejemplo, 5,100 hosts que comparten por alias un diccionario de 100 claves superan el límite de nodos.
- Un texto de `from_yaml` que supera un límite hace fallar la tarea que lo lee, y los demás hosts continúan. Un archivo que supera un límite detiene la ejecución antes de empezar, como hace un error de sintaxis.
- Un fact anidado a más profundidad que el límite de YAML no se escribe en la caché de facts `jsonfile`: un `set_fact` con `cacheable: true` imprime un aviso y conserva el fact solo durante la ejecución. El archivo contiene dos niveles de JSON por cada nivel del valor.

## Todavía no disponible

### Carencias de renderizado

- Una cadena que lee dos variables, una de ellas encadenada mediante una tercera que contiene un bloque `raw`, se renderiza una vez más que en ansible-core.
- Un bloque `{% set x %}...{% endset %}` cuyo cuerpo imprime una expresión hace que el resultado sea una cadena aunque el resto sea una sola expresión. Un bloque solo de texto conserva el tipo.
- `dict` solo y `dict.fromkeys` solo no son un resultado, como en ansible-core (`Type 'type' is unsupported for variable storage.`), y se imprimen como `<class 'dict'>` y `<built-in method fromkeys of type object>` dentro de un texto.
- Un `datetime` de `now()` participa en `+` y `-` con otro `datetime` o un `timedelta` y en ninguna otra aritmética, y le faltan los métodos `date`, `time`, `astimezone` y `timetuple`.

### Carencias de archivos de plantilla

- `template_host`, `template_uid`, `template_run_date` y `template_mtime` no están definidos, a diferencia de `template_path`, `template_fullpath` y `ansible_managed`.
- `line_statement_prefix` y `line_comment_prefix` en una línea `#jinja2:` se rechazan por su nombre. El motor de plantillas de Volant y Jinja2 difieren en una línea en blanco después de una sentencia de línea y en el salto de línea de un comentario de línea.
- Un delimitador de más de 64 caracteres y una línea `#jinja2:` de más de 16 KiB se rechazan con un error que nombra la opción. Los paréntesis en un valor de `#jinja2:` se anidan hasta 200 niveles, como en Python.
- Un `variable_end_string` o `block_end_string` que empieza por un espacio en blanco se rechaza por su nombre. Un delimitador vacío se rechaza también, donde Jinja2 lo interpreta de una forma que depende de la plantilla.
- Un `output_encoding` que no está entre los que enumera [Plantillas](/es/docs/variables/templating/), como `koi8-r`, `shift_jis` o `utf-7`, se rechaza por su nombre.

### Carencias de bytes

- Un valor bytes es una secuencia de enteros que se imprime como `b'abc'`. Un segmento y `+` (`b[1:]`, `b + b`) dan una lista de enteros, donde Python da bytes.
- Un valor bytes que contiene una variable (`set_fact`, `vars:`) se guarda como su texto: su `| length` cuenta caracteres y `.decode()` falla. Un bucle sobre elementos bytes muestra `(item=abc)` donde ansible-core muestra `(item=b'abc')`, porque un elemento de bucle se guarda como texto.
- Los métodos de bytes distintos de `decode` y `hex` (`upper`, `split`, `startswith`) fallan con su nombre.
- Un valor bytes que no es UTF-8 no puede ser un resultado ni imprimirse en un texto: ansible-core imprime `?` en su lugar. `to_yaml`, `dict2items`, `flatten`, `hash`, `checksum`, `md5`, `sha1` y todos los filtros que se ejecutan en Python rechazan los bytes por su nombre.
- `encode` y `decode` rechazan por su nombre las codificaciones distintas de las enumeradas arriba (`cp1252`, `shift_jis`), `decode` de `utf-16` y `utf-32`, y los manejadores de errores `namereplace`, `surrogateescape` y `surrogatepass` en `decode`.
