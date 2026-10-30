---
sourceHash: ac09abc44b15
title: Referenz der Template-Sprache
description: Die Python-Methoden, String-Regeln, regulären Ausdrücke und Grenzen des Jinja2 von Volant, mit jedem Unterschied zu ansible-core.
---

Diese Seite führt auf, was ein Template aufrufen kann und wo Volant von ansible-core abweicht. [Templating](/de/docs/variables/templating/) behandelt die alltäglichen Regeln und die Filter, Tests und Lookups, die Volant mitbringt.

## Datum und Fülltext

`now(utc=False, fmt=None)` liefert Datum und Uhrzeit auf dem Controller, nie auf einem verwalteten Host. Es gibt ein `datetime` ohne Zeitzone zurück, in UTC, wenn `utc` wahr ist. Ist `fmt` gesetzt, gibt es stattdessen den Text zurück, den `strftime(fmt)` liefert, sodass `{{ now(utc=true, fmt='%Y%m%d') }}` `20261019` ergibt. Das `datetime` hat die Attribute `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond` und `tzinfo`, die Methoden `strftime`, `isoformat`, `timestamp`, `weekday` und `isoweekday` und lässt sich mit einem anderen `datetime` vergleichen. Als Ergebnis oder über `to_json` wird es als `2026-10-19T23:13:50.741151` ausgegeben. Im Text, über `string` oder nach `~`, wird es wie in Python mit einem Leerzeichen statt des `T` ausgegeben. `strftime` verwendet immer die C-Locale: Tages- und Monatsnamen sind englisch, und `%c` ergibt `Mon Oct 19 23:13:50 2026`. ansible-core verwendet die Locale des Controllers.

`lipsum(n=5, html=True, min=20, max=100)` liefert `n` Absätze zufälligen Fülltext, wie Jinja es tut. Jeder hat mindestens `min` und weniger als `max` Wörter. Ist `html` wahr, steht jeder Absatz in `<p>`-Tags, und ein Zeilenumbruch trennt sie, sonst eine Leerzeile. Ein Aufruf erzeugt höchstens 10 Millionen Wörter.

## Python-Methoden

Ein Template kann die Methoden aufrufen, die Python seinen Werten gibt, wie in ansible-core. Eine Methode, die Volant nicht ausführt, schlägt mit ihrem eigenen Namen fehl, sodass nichts ohne Meldung übersprungen wird.

Diese `str`-Methoden laufen, mit den Argumenten und Keywords, die Python nimmt, und mit der Formulierung von Python bei einem falschen Aufruf (`must be str or None, not int`, `substring not found`):

| Gruppe | Methoden |
|---|---|
| Trennen und verbinden | `split`, `rsplit`, `splitlines`, `partition`, `rpartition`, `join`, `expandtabs` |
| Kürzen und auffüllen | `strip`, `lstrip`, `rstrip`, `removeprefix`, `removesuffix`, `center`, `ljust`, `rjust`, `zfill` |
| Suchen | `find`, `rfind`, `index`, `rindex`, `count`, `startswith`, `endswith`, `replace` |
| Groß- und Kleinschreibung | `upper`, `lower`, `title`, `capitalize`, `swapcase`, `casefold` |
| Prüfen | `isalpha`, `isalnum`, `isdigit`, `isdecimal`, `isnumeric`, `isspace`, `islower`, `isupper`, `istitle`, `isascii`, `isidentifier`, `isprintable` |
| Sonstige | `format`, `format_map`, `translate`, `maketrans`, `encode` |

Positionen zählen Zeichen, nicht Bytes, und `start` und `end` nehmen negative Werte. `split(maxsplit=1)` und die anderen Keyword-Formen funktionieren. `startswith` und `endswith` nehmen einen Text oder ein Tupel von Texten. Eine Liste wird als Tupel akzeptiert, weil das Jinja von Volant keinen Tupel-Typ hat.

Was von CPython abweicht:

- `casefold` ist Kleinschreibung plus `ß` zu `ss`. Es ist nicht das volle Unicode-Case-Folding.
- `isdigit`, `isalpha` und die anderen Prüfungen außerhalb von ASCII folgen den Unicode-Tabellen von Rust. `'½'.isdigit()` ist hier wahr und in Python falsch.
- Ein Format-Feld kann ein Element lesen (`{0[a]}`, `{a[0]}`), aber kein Attribut (`{0.real}`).
- Eine Breite, eine Anzahl oder eine Genauigkeit, die eine Methode mehr als 256 MiB Text aufbauen ließe (`'a'.ljust(10000000000)`, `'{:.300000000f}'.format(x)`), stoppt den Task mit einem Fehler, der die Grenze nennt. Python baut den Text auf, bis dem Rechner der Speicher ausgeht. Eine Zahl, die für einen Index zu groß ist, ergibt den eigenen Fehler von Python, `Python int too large to convert to C ssize_t`.
- `encode` erzeugt einen Bytes-Wert, und Volant führt nur wenige Methoden von Bytes aus. Siehe [Bytes](#bytes).
- Eine Backslash-Sequenz wie `'\n'` in einem Template-String wird von der Template-Engine gelesen. ansible-core 2.19 behält die beiden Zeichen. Übergeben Sie einen Wert in einer Variablen, wenn der Text einen echten Zeilenumbruch enthalten muss.

### Listen und Mappings

Auch die Methoden von `list` und `dict` laufen: `append`, `extend`, `insert`, `remove`, `pop`, `sort`, `reverse`, `index`, `count`, `copy` und `clear` bei einer Liste sowie `get`, `keys`, `values`, `items`, `pop`, `popitem`, `setdefault`, `update`, `copy`, `clear` und `fromkeys` bei einem Mapping. `fromkeys` ist auch eine Methode des Namens `dict`: `dict.fromkeys(['a', 'b'], 0)`. `keys()`, `values()` und `items()` liefern Listen. `sort` nimmt `reverse`, und `key` nur als `none`. Die üblichen Schreibweisen für eine Änderung sind `{% set _ = items.append(x) %}` und `{{ items.append(x) or '' }}`: Die Methode ändert die Liste und antwortet `none`.

Ein Template ändert eine Liste oder ein Mapping an Ort und Stelle, und andere Namen für denselben Wert sehen die Änderung:

```jinja
{% set seen = [] %}
{% for host in groups['web'] %}{% set _ = seen.append(host) %}{% endfor %}
{% set alias = seen %}{% set _ = alias.append('db') %}
{{ seen }}   {# every web host, then db #}
```

Das folgt Python und ansible-core 2.19, die in diesen Fällen übereinstimmen (jeder Fall wird gegen ansible-core getestet):

- Ein aus einer Variablen gelesener Wert ist für den String, der gerade gerendert wird, eine Kopie. Eine Play-Variable `x`, die ein Template mit `x.append(1)` ändert, wird nur für dieses Template geändert. Der nächste Task, das nächste Feld desselben Tasks und das nächste Template lesen sie, wie sie war. Eine Änderung an einer Variablen, die das Rendern überdauert, gibt es nicht.
- Innerhalb eines Templates benennt `b = a` eine Liste, und eine Änderung über `b` zeigt sich in `a`. `b = a | list` und `b = a.copy()` erzeugen eine neue Liste.
- Eine Liste oder ein Mapping in einer anderen Liste oder einem anderen Mapping wird über `x[0]`, `x.key` und `for item in x` als dasselbe Objekt erreicht, sodass `d.a.append(1)` `d` ändert. Ein Wert, der mit `append`, `extend`, `insert`, `update` oder `setdefault` gespeichert wird, ist eine Kopie, ebenso die Werte, die `get`, `items`, `values` und `copy` liefern.
- `d.setdefault('k', []).append(1)` ändert `d`. `v = d.setdefault('k', [])` gefolgt von `v.append(1)` ändert `d` hier, in ansible-core aber nicht.

Was von ansible-core abweicht:

- Eine Liste oder ein Mapping, das ein Template an ein Makro, `namespace()` oder einen anderen Aufruf übergibt, ist in Volant dasselbe Objekt. ansible-core übergibt eine Kopie, sodass ein Makro, das sein Argument ändert, die Liste des Aufrufers nur hier ändert.
- ansible-core 2.19 behält den Fehler eines Aufrufs, dessen Ergebnis nie gelesen wird. `{% set _ = [1].remove(5) %}` ist dort kein Fehler. Hier ist es einer.
- Eine Liste, die `a + b` oder ein Ausschnitt `a[1:]` aufgebaut hat, lässt sich nicht ändern: Schreiben Sie `(a + b) | list`. Der Fehler sagt das.
- Eine Liste, die ein Aufruf gerade aufgebaut hat und die keine Variable hält, etwa `(x | list).append(1)`, wird als Kopie geändert: Der Aufruf antwortet wie in Python, und sonst ändert sich nichts.
- Eine Liste, die in einem `{% for %}` über sich selbst wächst, wird bis zu ihrer Länge zu Beginn der Schleife gelesen. Eine, die `remove` oder `pop` in einer solchen Schleife schrumpfen lässt, schlägt mit einem undefinierten Wert fehl, wo ansible-core weitermacht.
- Eine Liste, die ein Filter liefert, ist wie in ansible-core eine Kopie, sodass `(items | first).append(1)` `items` nicht ändert. Ein Ausschnitt (`a[1:]`) teilt die darin liegenden Listen mit `a`.
- Eine Liste oder ein Mapping, das das Element oder der Schlüssel seiner eigenen Methode ist, wird als Kopie seiner selbst gelesen, und ein Mapping als Schlüssel ist ein Fehler (`unhashable type: 'dict'`), wie in Python.
- `sort()` einer Liste, die `nan` enthält, ist hier ein Fehler. ansible-core sortiert sie.
- `{% do %}` wird hier akzeptiert und von ansible-core 2.19 abgelehnt, das `jinja2.ext.do` nicht aktiviert. Verwenden Sie `{% set _ = ... %}` in einem Template, das auf beiden laufen muss.
- `items()` und `popitem()` liefern Listen, wo Python Tupel liefert, sodass sie als `['a', 1]` und nicht als `('a', 1)` ausgegeben werden.
- `sort(key=...)` mit einer Funktion ist nicht verfügbar.

Methoden wie `.get()`, `.keys()`, `.values()` und `.items()` funktionieren auch bei anderen mappingartigen Werten, etwa `hostvars` und der Schleifenvariablen.

### Bytes

`'text'.encode(encoding='utf-8', errors='strict')` erzeugt einen Bytes-Wert:

- `| length` zählt die Bytes. `b[0]`, `b[-1]`, `| first`, `| list` und `{% for %}` liefern Ganzzahlen.
- `b.decode(encoding, errors)` und `b.hex(sep, bytes_per_sep)` liefern Text. `.encode('ascii', 'ignore').decode('ascii')` entfernt die Zeichen, die nicht ASCII sind.
- `b64encode` liest genau die Bytes. `b64decode`, `regex_replace`, `regex_search`, `regex_findall` und `quote` lesen sie als UTF-8-Text.
- Ein Ergebnis, das Bytes sind, wird zu seinem Text, ebenso `x {{ b }}`. `'x' ~ b`, `b | string` und `'%s' % b` ergeben `b'abc'`, wie in Python.
- `==`, `is sequence`, `is string` und `type_debug` (`bytes`) antworten wie in ansible-core. `to_json` und `to_nice_json` schlagen mit `Object of type 'bytes' is not JSON serializable` fehl.

`encode` kennt `utf-8`, `ascii`, `latin-1`, `utf-16` und `utf-32` (auch `-le` und `-be`), unter den Aliasnamen, die Python liest (`UTF-8`, `utf8`, `iso-8859-1`). Die Fehlerbehandlungen sind `strict`, `ignore`, `replace`, `backslashreplace` und `xmlcharrefreplace`. `decode` kennt `utf-8`, `ascii` und `latin-1`, mit `strict`, `ignore`, `replace` und `backslashreplace`. Ein Fehler hat die Worte von Python: `'ascii' codec can't encode character '\xe9' in position 0: ordinal not in range(128)`.

### Der Operator `%`

Auf einem Text formatiert `%` ihn so, wie Python es tut: `'%s-%d' % (name, 3)`, `'%(host)s:%(port)d' % item`, `'%5.2f%%' % pct`. Gelesen werden die Flags `-`, `+`, Leerzeichen, `0` und `#`, eine Breite und eine Genauigkeit (jede davon als `*`, um sie aus den Argumenten zu lesen), die Längenmodifikatoren `h`, `l` und `L` und die Umwandlungen `s`, `r`, `a`, `c`, `d`, `i`, `u`, `o`, `x`, `X`, `e`, `E`, `f`, `F`, `g`, `G` und `%`. Ein falscher Aufruf schlägt mit den Worten von Python fehl: `not enough arguments for format string`, `not all arguments converted during string formatting`, `%d format: a real number is required, not str`.

Ganzzahlen bis 128 Bit, die ein Template schreibt oder berechnet, werden exakt gelesen (`'%d' % 18446744073709551615`). Eine Ganzzahl über 63 Bit in einer Variablendatei erreicht das Template als Text, was der YAML-Reader entscheidet, nicht `%`. Eine Breite oder eine Genauigkeit über 256 MiB Text ist wie bei den `str`-Methoden ein Task-Fehler, der die Grenze nennt, und eine über dem, was ein Index fasst, ist `width too big` von Python.

Auf zwei Zahlen übernimmt `%` das Vorzeichen des Divisors, wie in Python: `7 % -3` ist `-2`. Ein Divisor null ist ein Fehler. Der Filter `format` ist `%` mit ausgeschriebenen Argumenten und liest dieselben Formate.

Volant hat keinen Tupel-Typ, daher wird die rechte Seite von `%` als Tupel gelesen, wenn sie im Template ausgeschrieben ist, als `(a, b)`, `(a,)` oder `['a', 'b']`. Eine Liste, die eine Variable hält oder ein Aufruf liefert, ist wie in Python ein Argument: `'%s' % items` gibt die Liste aus. `'%s %s' % pair` mit einer Liste aus zwei Elementen in `pair` ist in ansible-core ein Fehler, und hier auch. Ein Tupel wird als Liste ausgegeben: `'%s' % ((1, 2),)` ist hier `[1, 2]` und in ansible-core `(1, 2)`.

Zwei Zahlen, die im Template ausgeschrieben sind (`7 % -3`), werden beim Kompilieren des Templates zusammengefasst, und diese Zusammenfassung folgt weder der Vorzeichenregel von Python noch löst sie bei einem Divisor null einen Fehler aus. Eine Zahl, die aus einer Variablen kommt, tut beides.

## Mengenfilter

`difference`, `intersect` und `union` bauen in ansible-core ein Python-`set` auf, dessen Reihenfolge ihre eigene ist: Kleine Ganzzahlen kommen zufällig sortiert heraus, und Strings kommen in einer Reihenfolge heraus, die sich mit `PYTHONHASHSEED` von Lauf zu Lauf ändert. Volant sortiert das Ergebnis, wenn jedes Element eine Ganzzahl ist, was jedem gemessenen Ganzzahlfall entspricht, und behält sonst die Reihenfolge, in der die Elemente zuerst auftauchten. Bei Strings ist nicht garantiert, dass diese Reihenfolge zu einem bestimmten Lauf von ansible-core passt.

## Strings mit Blöcken

Volant rendert jeden String, der `{{`, `{%` oder `{#` enthält, in einer Variablen, einem Fact, einem Task-Argument, einer Schleife oder einer Extra-Variablen. Es rendert sie wie ansible-core:

- `trim_blocks` ist an und `lstrip_blocks` aus. Der Zeilenumbruch nach einer `{% ... %}`-Zeile entfällt, sodass ein Block-Skalar mit einer `{% for %}`-Schleife pro Durchlauf eine Zeile ergibt und keine für die Tags.
- Ein Ergebnis aus genau einem `{{ }}` und sonst nichts behält seinen Typ, auch wenn Anweisungen oder Kommentare es umgeben. `{% set a = [1, 2] %}{{ a }}` ist eine Liste. Ein Ergebnis aus Text oder aus mehreren Ausdrücken ist ein String.
- Ein Ergebnis ganz ohne Ausgabe ist `none`. `{% if false %}x{% endif %}` und `{# note #}` sind `none`, kein leerer String.
- Ein Zeilenumbruch am Ende des Strings gehört nicht zum Ergebnis, wenn das Ergebnis kein String ist: `"{{ [1, 2] }}\n"` ist eine Liste. `{{-` und `-}}` entfernen den Leerraum um das Tag, sodass auch `{{- x -}}` mit einem Zeilenumbruch danach ein einziger Wert ist und das `-` nie ein Minuszeichen ist.
- `{#` beginnt einen Kommentar. Ein Kommentar, der ganz am Ende des Strings beginnt und nie schließt, wird akzeptiert und bedeutet nichts. Alles andere, was nicht abgeschlossen ist, ist ein Syntaxfehler. Um `{#` als Text zu schreiben, verwenden Sie `{{ '{#' }}`: Ein Ergebnis, das `{#` enthält, wird nicht erneut gerendert, ebenso wenig eine gespeicherte Variable oder ein Fact, dessen `{#` kein `#}` folgt.
- Ein String wie `echo ${#items[@]}` schlägt wie in ansible-core mit einem Syntaxfehler fehl. Umgeben Sie ihn mit `{% raw %}` oder markieren Sie ihn mit `!unsafe`. ansible-core lehnt einen `command`- oder `shell`-Task, der einen enthält, schon beim Laden der Tasks ab, mit Exit-Code 4. Volant lehnt ihn ab, wenn der Task läuft, mit Exit-Code 2, sodass die Tasks davor zuerst laufen.

Ein registriertes Ergebnis und jeder andere Text von einem verwalteten Host bleibt Text: Er wird nie gerendert. Siehe [Vertrauenswürdige und nicht vertrauenswürdige Werte](#vertrauenswürdige-und-nicht-vertrauenswürdige-werte). Ein Wert unter `!unsafe` ist Daten und wird nie als Template ausgewertet. Ein `!vault`-Wert wird entschlüsselt, siehe [Vault](/de/docs/variables/vault/).

## Reguläre Ausdrücke

`regex_replace`, `regex_search`, `regex_findall` und die Tests `match`, `search` und `regex` lesen Python-`re`-Muster. Volant schreibt das Muster für die Rust-Crate `regex` um und durchläuft die Treffer so, wie Python 3.7 und neuer es tut. Gruppenbehandlung, leere Treffer, Ersetzungsstrings (`\1`, `\g<name>`, ein literales `$`) und die Argumente, einschließlich der Flags nach Position, folgen ansible-core.

Manche Python-Syntax hat in der Rust-Crate keine Entsprechung. Ein Muster, das sie verwendet, schlägt mit einem Fehler fehl, der das Konstrukt nennt, wo ansible-core es ausführt:

- Look-ahead und Look-behind: `(?=`, `(?!`, `(?<=`, `(?<!`
- ein Rückbezug innerhalb des Musters: `\1`, `(?P=name)`
- possessive Quantifizierer (`a++`), atomare Gruppen (`(?>`) und bedingte Gruppen (`(?(1)a|b)`)
- `\N{name}`, die Flags `(?a)` und `(?L)` und `(?x:` als Flag mit begrenztem Geltungsbereich
- eine nicht gierige Wiederholung oder eine leere Alternative vor einer anderen (`a*?`, `|a`), wenn ein leerer Treffer einen längeren an derselben Stelle braucht
- `$` in einem Muster ohne `multiline`, auf einem Text mit mehr als einer Zeile, der mit einem Zeilenumbruch endet

Zwei Unterschiede bleiben. Ein Treffer mit mehreren Gruppen ist hier eine Liste von Listen und in ansible-core eine Liste von Tupeln, was sich nur zeigt, wenn der Wert als Text ausgegeben wird. Die Template-Engine von Volant liest `'\\'` in einem String-Literal als einen Backslash, wo ansible-core zwei behält.

## Literale geschweifte Klammern

Text, den ein `raw`-Block schreibt, oder ein String wie `'{{'` ist literal: Er wird als Argument, in einer Variablen, in einem `set_fact`-Wert oder in einem Schleifenelement nicht erneut gerendert. Eine Variable, die einen `raw`-Block hält, wird zuerst bis zu ihrem Ende gelesen, sodass `a: "{{ b }}"` mit `b: "{% raw %}{{ x }}{% endraw %}"` `{{ x }}` ergibt.

Auch Klammern, die ein Ausdruck aufbaut, sind literal. Mit `x: a` ergibt `{{ x ~ '{{ 1 + 1 }}' }}` `a{{ 1 + 1 }}`, `{{ x | replace('a', '{{ 7 * 6 }}') }}` ergibt `{{ 7 * 6 }}`, und `{{ '{{ ' ~ n ~ '_port }}' }}` ergibt `{{ web_port }}`, was auch immer `web_port` enthält. Eine Variable, die ein Template hält, wird beim Lesen gerendert, sodass eine Kette wie `a: "{{ b }}"` mit `b: "{{ c }}"` den Wert von `c` ergibt. Ein Ergebnis wird nur dann erneut gerendert, wenn ein Lesezugriff ihm ein noch nicht gerendertes Template gegeben hat: `hostvars['other']['name']` bei einer Variablen, die eines hält, oder eine Variable, die nach der obigen Regel wie geschrieben geblieben ist. Ein Text, der einen solchen Lesezugriff mit Klammern verbindet, die er aufbaut, wird dann als Ganzes gerendert.

## Filter, die in Python laufen

[Templating](/de/docs/variables/templating/#filter-tests-und-lookups-die-python-brauchen) sagt, welche Namen Python brauchen. Hier sind die Einzelheiten.

Ein Name in einer `import_playbook`-Zeile geht ebenfalls durch Python, gerendert mit den Variablen von `-e`.

Ein Python-Plugin, das eine Exception auslöst, schlägt mit der Kette von ansible-core fehl: der Satz des Plugins in einem Block, die Meldung der Exception im nächsten. Ein Filter oder Test, der eine Exception auslöst, gibt dieselbe Kette in einem Task-Argument, in einem `when` und in einer `template`-Datei aus.

Volant rendert zuerst jedes Argument und sendet das Ergebnis als Daten. Python wertet es nicht erneut als Template aus, sodass ein Wert, der von einem verwalteten Host kam und noch `{{ ... }}` enthält, einfacher Text bleibt. Auch Werte, die mit `!unsafe` markiert sind, kommen als Daten an.

Ein Lookup, das in Python läuft, erhält außerdem die Variablen des Plays. Volant liest den Quelltext des Plugins und benennt die Variablen, die es liest: die literalen Schlüssel, die es in `variables` nachschlägt, und die, die seine Einstellungen nennen. Ein Plugin, das Variablen auflistet, etwa `community.general.merge_variables`, erhält alle. Nur ein solches Plugin zahlt für das Senden jeder Variablen des Hosts.

Jedes der folgenden Ereignisse lässt den Task fehlschlagen, der den Aufruf gemacht hat, mit der Meldung des Python-Prozesses im Fehler des Tasks:

- Der Python-Prozess endet oder wird während eines Renderns beendet.
- Eine Anfrage oder eine Antwort ist größer als die Frame-Grenze.
- Ein Plugin antwortet mit Bytes, die kein UTF-8 sind.
- Der Python-Prozess antwortet für eine andere Anfrage.

Der nächste Aufruf im Lauf, der Python braucht, schlägt auf dieselbe Weise fehl, statt zu warten.

## Vertrauenswürdige und nicht vertrauenswürdige Werte
Ein Template, das Sie geschrieben haben, ist Code. Ein Wert, den ein verwalteter Host zurückgesendet hat, sind Daten. Volant hält die beiden so auseinander wie ansible-core 2.19, sodass ein String, den ein Host kontrolliert, nie als Template auf dem Controller läuft.

:::caution
Das ist eine Sicherheitsgrenze. Wenn Sie einen Weg finden, wie Host-Daten auf dem Controller als Template ausgewertet werden, melden Sie ihn vertraulich, wie in der [Sicherheitsrichtlinie](https://github.com/lorica-labs/volant/blob/main/SECURITY.md) beschrieben.
:::


### Welche Werte nicht vertrauenswürdig sind

| Nicht vertrauenswürdig, weil ein Host oder der Lauf sie erzeugt hat | Vertrauenswürdig, weil das Playbook sie trägt |
|---|---|
| `register`-Ergebnisse und Modulergebnisse | `vars` von Plays und Tasks |
| gesammelte Facts | `--extra-vars` |
| `set_fact`-Werte | Inventory-Variablen |
| `lookup('file')`, `lookup('pipe')`, `lookup('env')`, `lookup('template')` | `vars_files` und `include_vars` |

`!unsafe` markiert einen Wert des Playbooks als Daten. Es wird bei einem Wert und bei einem Mapping-Schlüssel akzeptiert, in einem Play, in `group_vars`, in einer Extra-Vars-Datei und in einem YAML-Inventory. Der Filter, den der YAML-Loader für die Markierung verwendet, ist für Playbooks verborgen: Ein Aufruf schlägt mit `No filter named 'volant_unsafe'` fehl.

### Was sich für einen nicht vertrauenswürdigen Wert ändert

Ein nicht vertrauenswürdiger Wert wird einmal gerendert. Sieht das Ergebnis wieder wie ein Template aus, bleibt es Text und wird kein zweites Mal gerendert, was Ansible auch tut:

```yaml
# Say someone wrote {{ lookup('pipe', 'id') }} into /etc/motd on the host.
- command: cat /etc/motd
  register: motd

- debug:
    msg: "{{ motd.stdout }}"   # prints the braces as text, runs nothing
```

`debug: var:` wertet einen Ausdruck aus. Stammt der Name selbst von einem Host, schlägt der Task mit der eigenen Meldung von ansible-core fehl:

```text
Task failed: Error while resolving `var` expression: Encountered untrusted template or expression.
```

Wenn Sie den Namen selbst schreiben, wie in `debug: var: result.stdout`, funktioniert das wie gewohnt. Es ist `debug: var: "{{ from_a_host }}"`, das anhält.

Bedingungen folgen derselben Regel. Ein `when: "{{ ... }}"`, das zu einem String gerendert wird, den Sie geschrieben haben, etwa `"1 == 1"`, wird noch einmal als Bedingung ausgewertet. Hat das Rendern etwas gelesen, das ein Host gesendet hat, wird der String nie kompiliert, und der Task schlägt mit demselben `Encountered untrusted template or expression.` fehl. Ein `assert`, dessen `that` ein Template ist, funktioniert genauso.

Auch das Ergebnis von `lookup('template')` sind Daten. Ein Template mit `{{ '{{ 1 + 1 }}' }}` ergibt unter beiden Engines den Text `{{ 1 + 1 }}`, nicht `2`.

Ein `delegate_to`, das aus einem Host-Wert gerendert wird, benennt einen Host mit diesem Text, was auch immer er enthält. Der Text ist der `inventory_hostname` und `inventory_hostname_short` des Delegationsziels, und nichts rendert ihn, auch wenn eine Variable Ihres Tasks Klammern daneben setzt. Dasselbe gilt für einen Namen, der Klammern enthält, woher er auch kam, und für ein Delegationsziel, das kein Host des Inventorys ist, etwa eines, das `add_host` angelegt hat. Über ssh schlägt der Task mit `UNREACHABLE` fehl und gibt den Text aus, wie in ansible-core. Ein Delegationsziel, das sich lokal verbindet, läuft wie gewohnt.

Ein Pfad, den ein verwalteter Host nennt, ist ein eigener Fall, weil der Controller ihn liest oder schreibt. Standardmäßig verhalten sich das `src` von `copy`, `template`, `script`, `unarchive` und `assemble` und das `dest` von `fetch` wie in ansible-core. Zwei Einstellungen verschärfen das: [`host_named_paths` für eine Quelldatei](/de/docs/internals/action-plugins/#eine-quelldatei-die-ein-verwalteter-host-benannt-hat) und [`host_named_dest_dirs` für ein `dest`](/de/docs/internals/action-plugins/#ein-dest-das-ein-verwalteter-host-benannt-hat).

### Wie fein die Unterscheidung ist

Volant verfolgt das Vertrauen pro Variable, nicht pro String. Ansible versieht jedes String-Objekt mit einer Markierung, sodass ein vertrauenswürdiger String sein Vertrauen durch ein Template behält und es nur verliert, wenn ein Ausdruck einen neuen String daraus baut. In Volant ist ein Wert nicht vertrauenswürdig, wenn das Rendern, das ihn erzeugt hat, etwas gelesen hat, das ein Host beigesteuert hat. Die Antwort ist überall dieselbe, wo ein Host beteiligt war. Volant ist großzügiger bei einem Wert, den das Playbook aus seinem eigenen Text berechnet hat.

Ein verwalteter Host kann außerdem seine eigenen Verbindungseinstellungen nicht über die Facts ändern, die ein Modul zurückgibt. Volant entfernt die Namen, die ansible-core entfernt, etwa `ansible_host`, `ansible_user` oder `ansible_ssh_common_args`, dazu `ansible_remote_tmp`, und gibt dieselbe Warnung aus.

## Grenzen für YAML
Volant lehnt einen YAML-Text ab, den es nicht ohne Risiko laden kann. Der Fehler nennt die Grenze. Ein Playbook, eine Variablendatei, ein Inventory und der Text, den ein `from_yaml`-Filter liest, folgen alle denselben Grenzen.

- Ein Dokument verschachtelt Collections höchstens 489 Ebenen tief. ansible-core lädt eine Datei bis 489 Ebenen und stoppt bei 490 mit `maximum recursion depth exceeded`, und Volant gibt dieselbe Meldung aus. Ein Alias zählt als die Ebenen des Knotens, den er benennt. Eine Flow-Collection stoppt früher, bei 255 Ebenen, weil der Scanner sie in einem `u8` zählt. ansible-core lädt eine Flow-Collection bis 489, sodass sich eine Datei mit vielen Flow-Collections zwischen 256 und 489 Ebenen weiterhin unterscheidet.
- Ein Text enthält ein Dokument, wie in ansible-core: Ein zweites `---` stoppt das Laden. Die folgenden Grenzen zählen den gesamten Text.
- Die Anker und Aliase eines Textes kopieren höchstens 1.000.000 Knoten und 64 MiB Skalartext. Eine Kopie entsteht an jedem Anker und an jedem Alias. ansible-core teilt einen Knoten und kopiert nichts, sodass es einen Text lädt, der für Milliarden Knoten steht, und erst fehlschlägt, wenn ein Task den Wert durchläuft. Zum Beispiel überschreiten 5.100 Hosts, die ein Mapping aus 100 Schlüsseln per Alias teilen, die Knotengrenze.
- Ein `from_yaml`-Text über einer Grenze lässt den Task fehlschlagen, der ihn liest, und die anderen Hosts machen weiter. Eine Datei über einer Grenze stoppt den Lauf vor dem Start, wie ein Syntaxfehler.
- Ein Fact, der tiefer verschachtelt ist als die YAML-Grenze, wird nicht in den Fact-Cache `jsonfile` geschrieben: Ein `set_fact` mit `cacheable: true` gibt eine Warnung aus und behält den Fact nur für den Lauf. Die Datei enthält zwei JSON-Ebenen für jede Ebene des Werts.

## Noch nicht vorhanden

### Lücken beim Rendern

- Ein String, der zwei Variablen liest, von denen eine über eine dritte verkettet ist, die einen `raw`-Block hält, wird einmal öfter gerendert als in ansible-core.
- Ein Block `{% set x %}...{% endset %}`, dessen Inhalt einen Ausdruck ausgibt, macht das Ergebnis zu einem String, auch wenn der Rest ein einziger Ausdruck ist. Ein Block nur aus Text behält den Typ.
- `dict` allein und `dict.fromkeys` allein sind kein Ergebnis, wie in ansible-core (`Type 'type' is unsupported for variable storage.`), und werden in einem Text als `<class 'dict'>` und `<built-in method fromkeys of type object>` ausgegeben.
- Ein `datetime` aus `now()` nimmt an `+` und `-` mit einem anderen `datetime` oder einem `timedelta` teil und an keiner anderen Arithmetik, und ihm fehlen die Methoden `date`, `time`, `astimezone` und `timetuple`.

### Lücken bei Template-Dateien

- `template_run_date` und `template_mtime` sind Text in der Form von Pythons `str` eines `datetime`, keine `datetime`-Werte: Sie werden gleich ausgegeben, und `type_debug` liefert `str`.
- `line_statement_prefix` und `line_comment_prefix` in einer `#jinja2:`-Zeile werden mit Namen abgelehnt. Die Template-Engine von Volant und Jinja2 unterscheiden sich bei einer Leerzeile nach einer Zeilenanweisung und beim Zeilenumbruch eines Zeilenkommentars.
- Ein Begrenzer, der länger als 64 Zeichen ist, und eine `#jinja2:`-Zeile, die länger als 16 KiB ist, werden mit einem Fehler abgelehnt, der die Option nennt. Klammern in einem `#jinja2:`-Wert verschachteln bis 200 Ebenen, wie in Python.
- Ein `variable_end_string` oder `block_end_string`, das mit Leerraum beginnt, wird mit Namen abgelehnt. Ein leerer Begrenzer wird ebenfalls abgelehnt, wo Jinja2 ihn je nach Template unterschiedlich liest.
- Ein `output_encoding`, das die Seite [Templates](/de/docs/variables/templating/) nicht nennt, etwa `koi8-r`, `shift_jis` oder `utf-7`, wird mit Namen abgelehnt.

### Lücken bei Bytes

- Ein Bytes-Wert ist eine Folge von Ganzzahlen, die als `b'abc'` ausgegeben wird. Ein Ausschnitt und `+` (`b[1:]`, `b + b`) liefern eine Liste von Ganzzahlen, wo Python Bytes liefert.
- Ein Bytes-Wert, den eine Variable hält (`set_fact`, `vars:`), wird als sein Text gespeichert: Sein `| length` zählt Zeichen, und `.decode()` schlägt fehl. Eine Schleife über Bytes-Elemente zeigt `(item=abc)`, wo ansible-core `(item=b'abc')` zeigt, weil ein Schleifenelement als Text gespeichert wird.
- Die Methoden von Bytes außer `decode` und `hex` (`upper`, `split`, `startswith`) schlagen mit ihrem Namen fehl.
- Ein Bytes-Wert, der kein UTF-8 ist, kann weder ein Ergebnis sein noch in Text ausgegeben werden: ansible-core gibt stattdessen `?` aus. `to_yaml`, `dict2items`, `flatten`, `hash`, `checksum`, `md5`, `sha1` und jeder Filter, der in Python läuft, lehnen Bytes mit Namen ab.
- `encode` und `decode` lehnen die Kodierungen, die nicht oben aufgeführt sind (`cp1252`, `shift_jis`), `decode` von `utf-16` und `utf-32` und die Fehlerbehandlungen `namereplace`, `surrogateescape` und `surrogatepass` bei `decode` mit Namen ab.
