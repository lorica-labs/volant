---
sourceHash: e421c9aa034e
title: Rollen, Includes und Imports
description: Wo Volant nach Rollen sucht, wie Abhängigkeiten, Argument-Specs und Rollenvariablen funktionieren und wie Imports und Includes andere Dateien in ein Play holen.
---

Diese Seite beantwortet, wo Volant eine Rolle findet, wie sich ihre Abhängigkeiten und Variablen verhalten und wie `import_*` und `include_*` andere Dateien einbinden. Volant liest das Verzeichnis einer Rolle, während das Play kompiliert wird, und fügt ihre Tasks in die Liste der Schritte ein. Nichts an einer Rolle wartet also auf einen Host.

## Eine Rolle finden

Einträge unter `roles:`, Abhängigkeiten in `meta/main.yml` und `import_role` nennen eine Rolle alle auf dieselbe Weise. Volant sucht in dieser Reihenfolge:

1. `<playbook_dir>/roles`
2. `roles_path` aus `ansible.cfg` oder `ANSIBLE_ROLES_PATH`. Ohne beides sind die Standardwerte `~/.ansible/roles`, `/usr/share/ansible/roles` und `/etc/ansible/roles`.
3. `<playbook_dir>` selbst

Ein relativer Eintrag in `roles_path` wird relativ zum Verzeichnis der `ansible.cfg` gelesen, oder relativ zum Arbeitsverzeichnis, wenn er aus `ANSIBLE_ROLES_PATH` stammt. Jeder Eintrag wird absolut gemacht, mit entferntem `.` und `..`, bevor er verwendet wird. Der `ansible_search_path` einer Rolle und die Zeilen `included:` zeigen diesen Pfad, wie ansible-core es tut.

Ein dreiteiliger Name wie `acme.demo.hello` wird auch unter `ansible_collections/acme/demo/roles/hello` gesucht, zuerst in einem Verzeichnis `collections/` neben dem Playbook, dann in jedem Eintrag von `collections_path`.

Eine Rolle, die niemand findet, bricht den Lauf mit Exit-Code 1 ab, und die Meldung listet jeden versuchten Pfad auf.

## Im Inneren einer Rolle

`defaults/` und `vars/` werden zuerst als Datei gelesen, dann als Verzeichnis: `vars/main.yml`, wenn es existiert, sonst jede Datei von `vars/main/` in Namensreihenfolge. `tasks/`, `handlers/` und `meta/` nehmen eine einzelne Datei und nie ein Verzeichnis. An einem Rolleneintrag wählen `tasks_from`, `vars_from`, `defaults_from` und `handlers_from` eine andere Datei als `main`.

Der Name ist ein Pfad unter dem passenden Verzeichnis, mit oder ohne Endung (`install`, `install.yml`, `sub/install`). Ein Name, der dieses Verzeichnis verlässt, etwa `../../../x` oder ein absoluter Pfad außerhalb davon, bricht den Lauf mit `Failed loading '...' for role (name) as it is not inside the expected role path` ab. Volant folgt einem symbolischen Link innerhalb des Verzeichnisses, wohin er auch zeigt, wie ansible-core.

Das Verzeichnis `handlers/` einer Rolle wird zusammen mit ihren Tasks gelesen. Bei einem Flush laufen die Handler der Rolle vor denen des Plays.

## Abhängigkeiten

Die `dependencies` in `meta/main.yml` sind eigenständige Rollen. Sie laufen vor der Rolle, die sie angefordert hat, und eine Abhängigkeit, die sich zwei Rollen teilen, läuft einmal. `allow_duplicates: true` schaltet diese Deduplizierung für eine Rolle ab.

Die Deduplizierung betrachtet mehr als den Namen. Die Parameter des Eintrags, seine `vars:`, sein `when:` und seine `*_from`-Auswahlen gehören alle zu seiner Identität:

```yaml
roles:
  - base                    # runs once...
  - base                    # ...because this is the same entry
  - { role: base, p: v }    # runs again: different parameters
```

## Argument-Specs

`meta/argument_specs.yml` wird zu einem eigenen Task. Er läuft nach den Abhängigkeiten der Rolle und vor ihrem ersten Task und prüft die Argumente der Rolle gegen den Spec-Eintrag, der nach ihrer Tasks-Datei benannt ist.

Der Task trägt die `short_description` der Rolle nach einem Bindestrich, sodass `--list-tasks` `Validating arguments against arg spec 'main' - The spec role` zeigt. Er ist mit `always` getaggt, daher entfernt ihn keine Auswahl mit `--tags`.

Die Prüfungen laufen in der Reihenfolge, in der Ansible sie ausführt: ein fehlendes Pflichtargument, dann ein Typ, dann ein Wert außerhalb von `choices`, dann ein Argument, das der Spec nicht nennt. Unteroptionen, `aliases`, `default` und `mutually_exclusive` werden noch nicht geprüft.

## Rollenvariablen

Eine Rolle liefert drei Schichten von Variablen, die in der [Rangfolge](/de/docs/variables/precedence/) auf unterschiedlicher Höhe liegen:

| Schicht | Wo sie liegt |
|---|---|
| `defaults/main.yml` | Ganz unten, unter den eigenen Variablen des Inventorys. |
| `vars/main.yml` | Über den `vars:` des Plays, unter denen eines Tasks. |
| Rollenparameter, die freien Schlüssel an einem Eintrag wie `{ role: base, p: v }` | Über den Facts. Ein Rollenparameter schlägt ein `set_fact` desselben Namens. |

:::tip
`vars:` an einem Rolleneintrag ist kein Rollenparameter. Es wird wie ein gewöhnliches Task-Keyword weitergereicht. Die beiden Schreibweisen sehen ähnlich aus, prüfen Sie also, welche Sie verwendet haben, wenn eine Rolle einen Wert liest, den Sie nicht erwartet haben.
:::

Die `defaults` und `vars` einer Rolle sind für das ganze Play sichtbar, sodass `pre_tasks` und spätere Rollen sie lesen können. Für die eigenen Schritte der Rolle werden ihre Werte wieder obenauf gelegt. Rollenparameter bleiben innerhalb der Rolle und der Rollen, von denen sie abhängt.

Eine Abhängigkeit liest auch, was die Rollen über ihr halten. Ihre `vars` liegen über der `vars/main.yml` ihrer Eltern und über der `vars/main.yml` ihrer eigenen Abhängigkeiten, die gegen die der Eltern gewinnt. Ihre `defaults` laufen umgekehrt: erst die ihrer Abhängigkeiten, dann die ihrer Eltern, dann ihre eigenen. Ihre Parameter liegen über den Parametern ihrer Eltern, sodass eine Abhängigkeit ohne Parameter dieses Namens den des nächsten Elternteils liest.

Ein `import_role` mit `public: false` behält sie für sich, und ein `include_role` tut das, sofern es nicht `public: true` sagt. Siehe [Rollenoptionen](#rollenoptionen).

## Imports und Includes

Fünf Anweisungen holen Arbeit aus einer anderen Datei herein. Sie fallen in zwei Familien:

| Anweisung | Art | Wann Volant die Datei liest | Was eine Auflistung zeigt |
|---|---|---|---|
| `import_tasks` | statisch | während das Play kompiliert wird | jeden Task darin |
| `import_role` | statisch | während das Play kompiliert wird | jeden Task darin |
| `import_playbook` | statisch | während das Play kompiliert wird | jedes Play darin |
| `include_tasks` | dynamisch | wenn ein Host die Anweisung erreicht | nur die Anweisung |
| `include_role` | dynamisch | wenn ein Host die Anweisung erreicht | nur die Anweisung |

Ein dynamisches Include wird pro Host aufgelöst, weil sein Dateiname von den Variablen dieses Hosts abhängen kann. Die neuen Schritte werden direkt hinter der Anweisung eingefügt, während das Play läuft.

### Statische Imports

Ein Pfad bei `import_tasks` wird zuerst neben der importierenden Datei gesucht, dann im eigenen Verzeichnis `tasks/` der Rolle.

`import_playbook` fügt die Plays der anderen Datei dort ein, wo die Anweisung steht, und jedes importierte Play behält das Verzeichnis der Datei, aus der es stammt. Ihr Dateiname wird mit den `-e`-Variablen und über den [Python-Pfad für Filter und Lookups](/de/docs/variables/templating/#filter-tests-und-lookups-die-python-brauchen) gerendert, sodass `{{ which }}.yml` mit `-e which=sub.yml` funktioniert, ebenso ein Name, der mit `lookup('env', ...)` oder einem Collection-Filter gebildet wird. Noch gibt es kein Inventory, ein Name kann also nicht von einem Host abhängen. Ein Name, der eine undefinierte Variable liest, bricht das Laden mit Exit 4 ab: `Error processing keyword 'import_playbook': 'nosuchvar' is undefined`.

Drei Keywords des Eintrags `import_playbook` erreichen jedes Play der Datei:

- `tags` werden zu den Tags jedes importierten Plays hinzugefügt, sodass `--tags` und `--skip-tags` seine Tasks auswählen und die Auflistungen sie zeigen.
- `vars` werden zu den Variablen jedes importierten Plays hinzugefügt und schlagen die eigenen des Plays.
- `when` wird vor die Bedingungen jedes Tasks, jeder Rolle und jedes Blocks jedes importierten Plays gesetzt, ebenso vor sein Sammeln von Facts. Es wird nicht einmal für den gesamten Import ausgewertet. Eine falsche Bedingung öffnet trotzdem jedes Play und gibt trotzdem für jeden Task und Host ein Überspringen aus. Handler sind nicht bedingt.

Die übrigen Keywords, die ansible-core 2.19 am Eintrag akzeptiert, etwa `ignore_errors`, `become` und `environment`, bewirken dort nichts, und Volant ignoriert sie genauso. Jeder andere Schlüssel wird abgelehnt. Ein `tags` oder `when`, das ein Template enthält, wird ebenfalls abgelehnt, weil Volant beim Laden des Playbooks keine Variablen hat.

### Dynamische Includes

Von dem, was die Anweisung selbst sagt, gelangen nur ihre `vars:` in die eingebundene Datei. Diese Variablen treten auf der Ebene der Rollenparameter in die [Rangfolge](/de/docs/variables/precedence/) ein, über den Facts und unter `--extra-vars`, und sie sammeln sich über verschachtelte Includes an. Die eigenen `tags` der Anweisung enden an der Anweisung, und ihr `when` wird nur für die Anweisung ausgewertet.

Was die Schichten um die Anweisung herum sagen, erreicht dagegen die eingebundenen Tasks: das Play, der Rolleneintrag, der die Datei eingebracht hat, und die Blöcke darum. Ihre `tags`, `become`, `become_user`, `no_log`, `when`, `environment`, `delegate_to`, `ignore_errors`, `notify` und `vars` gelten für jeden eingebundenen Task, und der eigene Wert eines Tasks schlägt einen geerbten. Das ist die Regel von ansible-core 2.19, die ein dynamisch geladenes Elternelement überspringt und das Großelternelement fragt. Ein geerbtes `check_mode: true` wird in der eingebundenen Datei abgelehnt statt verworfen.

Ein eingebundener Task landet in dem Block und Abschnitt, in dem die Anweisung geschrieben wurde, sodass ein fehlschlagendes Include im Rumpf eines Blocks vom `rescue` dieses Blocks aufgefangen wird.

Volant prüft, was ein Include einbringt, bevor es läuft. Zu diesem Zeitpunkt sind die Hosts verbunden und frühere Tasks gelaufen, sodass ein Modul oder Keyword, das dieses Release noch nicht unterstützt, nicht mehr den ganzen Lauf abbrechen kann. Stattdessen lässt es die Include-Anweisung für den Host fehlschlagen, der sie erreicht hat, nichts in der Datei läuft, und ein `rescue` um die Anweisung kann es auffangen.

Vor der ersten Verbindung sammelt Volant die Python-Module, die jede Task- und Handler-Datei der Rollen im Play nennt. Ein Modul, das nur in `setup-{{ ansible_os_family }}.yml` genannt wird, liegt daher bereits auf dem Host, wenn das Include läuft. Eine Datei unter `tasks/` oder `handlers/` einer Rolle, die sich nicht parsen lässt, bricht den Lauf für `roles:` und `import_role` ab, bevor sich etwas verbindet. Bei einem `include_role` liest dieser frühe Scan die Rolle nicht: Frühere Plays und Tasks laufen, dann endet der Lauf nach dem Banner der Anweisung mit Exit 4, und ein `when: false` am Include verbirgt den Fehler. Siehe [Der warme Python-Pfad](/de/docs/internals/python/#wie-es-funktioniert).

Ein `notify` in einer eingebundenen Datei, das keinen Handler nennt, beendet den Lauf wie in ansible-core mit Exit-Code 1 und demselben Satz, den die Vorabprüfung liefert. Die Vorabprüfung ist der Scan, den Volant ausführt, bevor es sich mit einem Host verbindet. Kein `ignore_errors` und kein `rescue` kann das auffangen. Siehe [Blöcke und Handler](/de/docs/playbooks/blocks/#unterschiede-zu-ansible).

#### Keywords auf das setzen, was ein Include einbringt

`apply:` bei `include_tasks` und `include_role` setzt Keywords auf jeden Task, den die Anweisung einbringt, und auf die Handler einer eingebundenen Rolle. Es nimmt die Keywords eines Blocks: `tags`, `when`, `vars`, `become`, `environment`, `ignore_errors`, `delegate_to` und den Rest. Die Imports lehnen `apply` ab, da sie die Keywords, die Sie an ihnen schreiben, bereits weitergeben.

```yaml
- include_tasks:
    file: install.yml
    apply:
      tags: [install]
      become: true
  tags: [always]
```

Die Tags in `apply` liegen an den eingebundenen Tasks und nicht an der Anweisung. Mit `--tags install` läuft die Anweisung nur, wenn sie einen Tag trägt, den die Auswahl behält, hier `always`, und dann laufen die eingebundenen Tasks. Ein `when` in `apply` wird für jeden eingebundenen Task geprüft, ein `when` an der Anweisung einmal. Die `vars` in `apply` sind Block-Variablen, daher schlagen die eigenen `vars:` der Anweisung sie.

Ein Schlüssel in `apply`, den ein Block nicht hat, bricht den Lauf wie in ansible-core mit Exit-Code 4 ab, wenn die Anweisung läuft, nach ihrem Banner. Ein `apply`, das kein Mapping ist, bricht das Laden ab.

#### Rollenoptionen

Mit `public: true` sind die `defaults` und `vars` eines `include_role` für alles sichtbar, was nach der Anweisung läuft, und auch für die anderen Hosts, weil ansible-core die Liste der öffentlichen Rollen im Play führt und nicht im Host. Ein Task, der vor der Anweisung lief, liest nichts davon, und ein Include mit `when: false` stellt nichts bereit.

Mit `allow_duplicates: false` zeigt die Anweisung ihre Zeile `included:` und führt für einen Host nichts aus, dem bereits eine identische Rolle eingebracht wurde. Zwei Anweisungen sind identisch, wenn sie dieselbe Rolle, dieselben Dateien und dieselben `vars:` nennen, und bei einer Schleife dasselbe Element unter derselben Schleifenvariable. `rolespec_validate: false` lässt die Prüfung des Argument-Specs aus.

Jede Option der vier Anweisungen, wie Volant und ansible-core 2.19.12 sie lesen:

| Anweisung | Option | Standard | Volant |
|---|---|---|---|
| `include_role` | `name`, `role` | erforderlich | funktioniert |
| `include_role` | `tasks_from`, `vars_from`, `defaults_from`, `handlers_from` | `main` | funktioniert |
| `include_role` | `public` | `false` | funktioniert |
| `include_role` | `allow_duplicates` | `true` | funktioniert |
| `include_role` | `rolespec_validate` | `true` | funktioniert |
| `include_role` | `apply` | keines | funktioniert |
| `import_role` | `name`, `role`, die vier `*_from` | wie oben | funktioniert |
| `import_role` | `public` | `true` | funktioniert: `false` hält die Variablen in der Rolle |
| `import_role` | `allow_duplicates` | `true` | funktioniert, entschieden während das Play kompiliert wird |
| `import_role` | `rolespec_validate` | `true` | funktioniert |
| `import_role` | `apply` | keines | wie in ansible-core abgelehnt, `Invalid options for import_role: apply` |
| `include_tasks` | `file` oder der freie Name | erforderlich | funktioniert |
| `include_tasks` | `apply` | keines | funktioniert |
| `import_tasks` | `file` oder der freie Name | erforderlich | funktioniert |
| `import_tasks` | `apply` | keines | wie in ansible-core abgelehnt, `Invalid options for import_tasks: apply` |

Ein Wert in `public`, `allow_duplicates` oder `rolespec_validate`, der kein Boolean ist, lässt den Task fehlschlagen, oder bricht bei einem `import_role` das Laden ab, mit dem Satz von ansible-core.

### include_vars

`include_vars` erzeugt Variablen statt Schritte und ist daher weder statisch noch dynamisch. Es ist ein gewöhnliches Modul, das auf dem Controller läuft, und was es liest, liegt wie in Ansible unter `set_fact` und registrierten Ergebnissen.

| Frage | Antwort |
|---|---|
| Was liest es? | Eine Datei, genannt über `file:` oder als freier Pfad geschrieben, oder jede Datei eines Verzeichnisses, genannt über `dir:`. Die beiden Formen nehmen unterschiedliche Optionen, und Optionen beider Formen lassen den Task fehlschlagen. |
| Wo wird eine Datei gesucht? | In `vars/` und dann im Verzeichnis selbst, für eine Rolle und dann für das Playbook. Ein absoluter Pfad wird unverändert verwendet. |
| Wo wird ein `dir:` gesucht? | Es wird nicht gesucht. Ein Task einer Rolle liest es unter dem `vars/` der Rolle, und `dir: vars/x` nennt ein Verzeichnis der Rolle selbst, wenn es existiert. Jeder andere Task liest es relativ zum Verzeichnis der Datei, die den Task enthält. |
| In welcher Reihenfolge wird ein Verzeichnis gelesen? | Verzeichnisse nach ihrem vollständigen Pfad als Text sortiert, dann die Dateien jedes Verzeichnisses nach Namen sortiert. Versteckte Dateien und Verzeichnisse werden gelesen, und Links auf Verzeichnisse werden verfolgt. |
| Was, wenn zwei Dateien denselben Schlüssel setzen? | Eine spätere Datei ersetzt die Schlüssel einer früheren vollständig. |
| Was steht im Ergebnis? | `ansible_facts` und `ansible_included_var_files`, die Dateien in der Reihenfolge, in der sie gelesen wurden. Eine leere Datei fügt nichts hinzu und wird trotzdem aufgelistet. |
| Was ist mit einer Vault-Datei? | Ein Task, der eine liest, wird wie ein `no_log`-Task verborgen: `-v` zeigt die zensierte Zeile, und ein Schleifenelement, das eine gelesen hat, zeigt `(censored due to no_log)`. |
| Check-Modus? | Der Task läuft im Check-Modus, wie in Ansible. |
| Was schlägt fehl? | Ein fehlendes Verzeichnis, ein Pfad, der kein Verzeichnis ist, eine Datei ohne gültige Endung und eine Datei, die kein Mapping ist, lassen den Task wie in Ansible mit gesetzter `message` fehlschlagen. Ein Fehler in einem Verzeichnis behält die Variablen der Verzeichnisse vor dem fehlschlagenden im Ergebnis, aber ein fehlgeschlagener Task setzt keine Variablen. Eine Datei, die sich nicht parsen oder entschlüsseln lässt, löst stattdessen den Fehler aus, und der Task hat kein eigenes Ergebnis. Eine Option vom falschen Typ schlägt mit dem eigenen Satz von Python fehl. |

Die Optionen der Form `dir:`:

| Option | Bedeutung |
|---|---|
| `dir` | Das Verzeichnis, das gelesen wird. |
| `depth` | Wie viele Ebenen gelesen werden, wobei das Verzeichnis die erste ist. `0`, der Standard, liest alle. |
| `files_matching` | Ein regulärer Ausdruck, der in jedem Dateinamen gesucht wird, nicht in seinem Pfad. Dateien, die nicht passen, werden ausgelassen. |
| `ignore_files` | Eine Liste regulärer Ausdrücke oder ein String daraus, an Leerraum getrennt. Jeder ist am Ende des Namens verankert. Dateien, die passen, werden ausgelassen. |
| `extensions` | Die Endungen, die eine Datei haben darf, ohne Punkt. Der Standard ist `yaml`, `yml` und `json`. Der Vergleich beachtet die Groß-/Kleinschreibung. |
| `ignore_unknown_extensions` | Lässt eine Datei mit einer anderen Endung aus. Ohne diese Option lässt eine solche Datei den Task fehlschlagen. |
| `name` | Legt alles unter diese eine Variable. Funktioniert auch für eine einzelne Datei. |
| `hash_behaviour` | `merge` legt jede Variable über die Variable desselben Namens, die der Task bereits sieht, Mappings bis ganz nach unten zusammengeführt. Jeder andere Wert ersetzt. |

### Verschachtelungsgrenze

Eine Anweisung auf Ebene 32 wird abgelehnt. Ein Zähler deckt die Rollenverschachtelung ab, `import_role` in den Tasks einer Rolle und `import_tasks` in einer importierten Datei, weil ein Zyklus durch jede Mischung der drei laufen kann.

## Unterschiede zu Ansible


- Ein Rollenname mit einem `.` wird als Pfad versucht, bevor er als Collection-Rolle versucht wird, sodass ein Rollenverzeichnis mit einem Punkt im Namen erreichbar bleibt.
- `collections_path` aus `ansible.cfg` und `ANSIBLE_COLLECTIONS_PATH` werden genauso gelesen wie `roles_path`. Dieses Verhalten wurde nicht gegen ansible-core gemessen. Das Verzeichnis `collections/` neben dem Playbook wurde gemessen.
- Eine fehlende `import_tasks`-Datei wird mit den ersten beiden Sätzen von ansible-core gemeldet, aber nicht mit dessen drittem, der eine Python-Fehlernummer für ein Modul nennt, das nie aufgerufen wurde.
- Ein als Task geschriebenes `import_playbook` wird abgelehnt, bevor das Play startet, statt unter einem Play-Banner fehlzuschlagen. Der Exit-Code ist derselbe 2.
- Eine Datei, die sich selbst importiert, wird mit Exit-Code 1 und einer eigenen Meldung abgelehnt. Ansible rekursiert, bis der Interpreter aufgibt, bei Exit-Code 250 mit einem langen Traceback.
- Eine fehlschlagende Prüfung des Argument-Specs hat `"failed": true` in ihrem Ergebnis, wie jeder Fehler auf dem Controller in Volant.


- Eine Datei, die sich selbst einbindet, trifft auf diese Grenze und wird als Task-Fehler mit Exit-Code 2 gemeldet. Ansible rekursiert, bis der Python-Stack erschöpft ist, bei Exit-Code 250.
- Eine eingebundene Datei außerhalb von `tasks/` und `handlers/` einer Rolle, die sich nicht parsen lässt, lässt den Task mit Exit-Code 2 und einem Recap fehlschlagen. Ansible endet mit Code 4 und ohne Recap. Ein Fehler, den der Recap mitzählt, ist nützlicher, und ein `rescue` kann ihn auffangen.
- Ein fehlschlagendes Include in einer Schleife gibt die übliche Zeile `failed: [host] (item=...)` aus. Ansible gibt nur ein `[ERROR]` auf der Standardfehlerausgabe aus.
- `allow_duplicates: false` zählt eine Rolle als für einen Host ausgeführt, wenn ihre Tasks eingebracht werden. ansible-core zählt sie, wenn der Host mindestens einen ihrer Tasks ausgeführt hat, sodass eine Rolle, deren Tasks alle übersprungen werden, dort erneut läuft.
- `allow_duplicates: false` bei einem `import_role` vergleicht die Anweisungen, während das Play kompiliert wird, ein `when` an der früheren Anweisung ändert das Ergebnis für einen Host daher nicht.
